//! Core host engine for spaph: plugin manager and command dispatch.

use spaph_plugin::{PluginFactory, PluginInfo as SpaphPluginInfo, PluginInstance, format_from_path};
use spaph_types::{PluginFormat, Result, SpaphError};
use spaph_vst2::Vst2Factory;
use spaph_vst3::Vst3Factory;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Debug, Clone)]
pub struct PluginSummary {
    pub id: u32,
    pub alias: Option<String>,
    /// Plugin whose output feeds this plugin's input (`plugin connect`), if any.
    pub input_from: Option<u32>,
    pub name: String,
    pub vendor: String,
    pub format: PluginFormat,
    pub num_inputs: u32,
    pub num_outputs: u32,
    pub num_params: usize,
    pub path: PathBuf,
}

/// Check that `alias` can be used as a plugin name: a plain word (ASCII letters, digits,
/// `_` or `-`), not starting with a digit (so it cannot be mistaken for an id), and not the
/// keyword `all`. Matching is case-sensitive.
pub fn validate_alias(alias: &str) -> Result<()> {
    let first_ok = alias.chars().next().map_or(false, |c| c.is_ascii_alphabetic() || c == '_');
    let rest_ok = alias.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !first_ok || !rest_ok {
        return Err(SpaphError::Plugin(format!(
            "invalid alias '{}': use letters, digits, '_' or '-', starting with a letter or '_'",
            alias
        )));
    }
    if alias == "all" {
        return Err(SpaphError::Plugin("alias 'all' is reserved".into()));
    }
    Ok(())
}

pub struct LoadedPlugin {
    pub id: u32,
    pub alias: Option<String>,
    /// Source plugin feeding this plugin's audio input. Explicit only (`plugin connect`).
    pub input_from: Option<u32>,
    pub path: PathBuf,
    pub info: SpaphPluginInfo,
    pub instance: std::sync::Arc<std::sync::Mutex<Box<dyn PluginInstance>>>,
    pub midi_queue: std::sync::Arc<std::sync::Mutex<Vec<spaph_types::MidiEvent>>>,
    pub gain: std::sync::Arc<std::sync::Mutex<f32>>,
    pub midi_connections: Vec<(String, Option<u8>)>,
    pub audio_device: Option<String>,
}

pub struct PluginManager {
    plugins: Vec<LoadedPlugin>,
    next_id: AtomicU32,
}

impl PluginManager {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            next_id: AtomicU32::new(1),
        }
    }

    fn factory_for(path: &Path) -> Option<Box<dyn PluginFactory>> {
        let fmt = format_from_path(path).or_else(|| {
            if path.is_dir() {
                Some(PluginFormat::Vst3)
            } else {
                None
            }
        })?;
        match fmt {
            PluginFormat::Vst2 => Some(Box::new(Vst2Factory)),
            PluginFormat::Vst3 => Some(Box::new(Vst3Factory)),
        }
    }

    pub fn load(&mut self, path: &Path) -> Result<u32> {
        self.load_as(path, None)
    }

    /// Load a plugin, optionally giving it an alias usable in place of its id.
    /// The alias is validated and checked for uniqueness before the plugin is loaded.
    pub fn load_as(&mut self, path: &Path, alias: Option<&str>) -> Result<u32> {
        if let Some(a) = alias {
            validate_alias(a)?;
            if self.plugins.iter().any(|p| p.alias.as_deref() == Some(a)) {
                return Err(SpaphError::Plugin(format!("alias '{}' already in use", a)));
            }
        }
        let factory = Self::factory_for(path)
            .ok_or_else(|| SpaphError::Plugin("unknown plugin format".into()))?;
        if !factory.can_load(path) {
            // still attempt
        }
        let instance = factory.load(path)?;
        let info = instance.info().clone();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.plugins.push(LoadedPlugin {
            id,
            alias: alias.map(str::to_string),
            input_from: None,
            path: path.to_path_buf(),
            info,
            instance: std::sync::Arc::new(std::sync::Mutex::new(instance)),
            midi_queue: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            gain: std::sync::Arc::new(std::sync::Mutex::new(1.0)),
            midi_connections: Vec::new(),
            audio_device: None,
        });
        Ok(id)
    }

    pub fn unload(&mut self, id: u32) -> Result<()> {
        let pos = self
            .plugins
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| SpaphError::Plugin(format!("no plugin with id {}", id)))?;
        self.plugins.remove(pos);
        // Links that used this plugin as a source now have no input.
        for p in self.plugins.iter_mut() {
            if p.input_from == Some(id) {
                p.input_from = None;
            }
        }
        Ok(())
    }

    fn position_of(&self, id: u32) -> Option<usize> {
        self.plugins.iter().position(|p| p.id == id)
    }

    /// Make `source`'s output the audio input of `target`. Replaces any previous source of
    /// `target`. Fan-out (one source, several targets) is allowed. Nothing changes on error.
    pub fn connect(&mut self, source: u32, target: u32) -> Result<()> {
        if self.position_of(source).is_none() {
            return Err(SpaphError::Plugin(format!("no plugin with id {}", source)));
        }
        let dst = self.position_of(target)
            .ok_or_else(|| SpaphError::Plugin(format!("no plugin with id {}", target)))?;
        if source == target {
            return Err(SpaphError::Plugin(format!("plugin {} cannot feed itself", source)));
        }
        if self.plugins[dst].info.num_inputs == 0 {
            return Err(SpaphError::Plugin(format!("plugin {} has no audio inputs", target)));
        }
        // Walk upstream from the source: a link makes `target` a source of itself if the chain
        // already reaches it. Each plugin has one source, so the walk is a simple list.
        let mut cur = Some(source);
        let mut steps = 0;
        while let Some(id) = cur {
            if id == target {
                return Err(SpaphError::Plugin(format!(
                    "connecting {} to {} would create a cycle", source, target
                )));
            }
            steps += 1;
            if steps > self.plugins.len() {
                break;
            }
            cur = self.plugins[self.position_of(id).unwrap()].input_from;
        }
        self.plugins[dst].input_from = Some(source);
        Ok(())
    }

    /// Remove the audio input link of `target`.
    pub fn disconnect(&mut self, target: u32) -> Result<()> {
        let dst = self.position_of(target)
            .ok_or_else(|| SpaphError::Plugin(format!("no plugin with id {}", target)))?;
        if self.plugins[dst].input_from.take().is_none() {
            return Err(SpaphError::Plugin(format!("plugin {} has no input connection", target)));
        }
        Ok(())
    }

    /// The source feeding `id`, if any.
    pub fn input_source(&self, id: u32) -> Option<u32> {
        self.position_of(id).and_then(|i| self.plugins[i].input_from)
    }

    /// All links as `(target, source)` pairs.
    pub fn links(&self) -> Vec<(u32, u32)> {
        self.plugins
            .iter()
            .filter_map(|p| p.input_from.map(|s| (p.id, s)))
            .collect()
    }

    /// Resolve a command argument to a plugin id. A numeric token is the id itself; anything
    /// else is looked up as an alias (case-sensitive). Returns `None` if no plugin matches.
    pub fn resolve(&self, token: &str) -> Option<u32> {
        if let Ok(id) = token.parse::<u32>() {
            return Some(id);
        }
        self.plugins.iter().find(|p| p.alias.as_deref() == Some(token)).map(|p| p.id)
    }

    pub fn list(&self) -> Vec<PluginSummary> {
        self.plugins
            .iter()
            .map(|p| PluginSummary {
                id: p.id,
                alias: p.alias.clone(),
                input_from: p.input_from,
                name: p.info.name.clone(),
                vendor: p.info.vendor.clone(),
                format: p.info.format,
                num_inputs: p.info.num_inputs,
                num_outputs: p.info.num_outputs,
                num_params: p.info.params.len(),
                path: p.path.clone(),
            })
            .collect()
    }

    pub fn get_instance(&self, id: u32) -> Option<std::sync::Arc<std::sync::Mutex<Box<dyn PluginInstance>>>> {
        self.plugins.iter().find(|p| p.id == id).map(|p| p.instance.clone())
    }

    pub fn get_midi_queue(&self, id: u32) -> Option<std::sync::Arc<std::sync::Mutex<Vec<spaph_types::MidiEvent>>>> {
        self.plugins.iter().find(|p| p.id == id).map(|p| p.midi_queue.clone())
    }

    pub fn list_presets(&self, id: u32) -> Result<Vec<String>> {
        let p = self.plugins.iter().find(|p| p.id == id)
            .ok_or_else(|| SpaphError::Plugin(format!("no plugin with id {}", id)))?;
        Ok(p.info.presets.clone())
    }

    pub fn select_preset(&mut self, id: u32, index: usize) -> Result<()> {
        let p = self.plugins.iter_mut().find(|p| p.id == id)
            .ok_or_else(|| SpaphError::Plugin(format!("no plugin with id {}", id)))?;
        if index >= p.info.presets.len() {
            return Err(SpaphError::Plugin("preset index out of range".into()));
        }
        p.instance.lock().unwrap().select_preset(index)
    }

    pub fn get_gain(&self, id: u32) -> Result<f32> {
        let p = self.plugins.iter().find(|p| p.id == id)
            .ok_or_else(|| SpaphError::Plugin(format!("no plugin with id {}", id)))?;
        Ok(*p.gain.lock().unwrap())
    }

    pub fn set_gain(&self, id: u32, value: f32) -> Result<()> {
        let p = self.plugins.iter().find(|p| p.id == id)
            .ok_or_else(|| SpaphError::Plugin(format!("no plugin with id {}", id)))?;
        *p.gain.lock().unwrap() = value;
        Ok(())
    }

    pub fn get_gain_handle(&self, id: u32) -> Option<std::sync::Arc<std::sync::Mutex<f32>>> {
        self.plugins.iter().find(|p| p.id == id).map(|p| p.gain.clone())
    }

    pub fn add_midi_connection(&mut self, id: u32, port: String, channel: Option<u8>) -> std::result::Result<(), String> {
        let p = self.plugins.iter_mut().find(|p| p.id == id)
            .ok_or_else(|| format!("no plugin with id {}", id))?;
        // reject a conflicting connection on the same port:
        // - an "all channels" connection conflicts with any other on that port
        // - two connections with the same channel conflict
        for (pp, cc) in &p.midi_connections {
            if pp == &port && (cc.is_none() || channel.is_none() || *cc == channel) {
                return Err(format!(
                    "plugin {} is already connected to '{}' in a conflicting way (all-channels or same channel)",
                    id, port
                ));
            }
        }
        p.midi_connections.push((port, channel));
        Ok(())
    }

    pub fn remove_midi_connection(&mut self, id: u32, port: &str, channel: Option<u8>) {
        if let Some(p) = self.plugins.iter_mut().find(|p| p.id == id) {
            p.midi_connections.retain(|(pp, cc)| !(pp == port && *cc == channel));
        }
    }

    pub fn clear_midi_connections(&mut self, id: u32) {
        if let Some(p) = self.plugins.iter_mut().find(|p| p.id == id) {
            p.midi_connections.clear();
        }
    }

    pub fn midi_connections(&self, id: u32) -> Vec<(String, Option<u8>)> {
        self.plugins.iter().find(|p| p.id == id)
            .map(|p| p.midi_connections.clone())
            .unwrap_or_default()
    }

    pub fn clear_all_midi_connections(&mut self) {
        for p in &mut self.plugins {
            p.midi_connections.clear();
        }
    }

    pub fn set_audio_device(&mut self, id: u32, name: String) {
        if let Some(p) = self.plugins.iter_mut().find(|p| p.id == id) {
            p.audio_device = Some(name);
        }
    }

    pub fn clear_audio_device(&mut self, id: u32) {
        if let Some(p) = self.plugins.iter_mut().find(|p| p.id == id) {
            p.audio_device = None;
        }
    }

    pub fn clear_all_audio_devices(&mut self) {
        for p in &mut self.plugins {
            p.audio_device = None;
        }
    }

    pub fn audio_device_name(&self, id: u32) -> Option<String> {
        self.plugins.iter().find(|p| p.id == id).and_then(|p| p.audio_device.clone())
    }

    pub fn count(&self) -> usize {
        self.plugins.len()
    }

    /// Remove every plugin and return them, so the caller decides when they are dropped.
    /// Dropping a plugin runs its full VST3/VST2 teardown.
    pub fn take_all(&mut self) -> Vec<LoadedPlugin> {
        std::mem::take(&mut self.plugins)
    }
}

/// Run `f` on a helper thread and wait at most `timeout` for it to finish.
/// Returns `true` if it finished in time. On timeout the thread is left running
/// (the caller is expected to exit the process).
pub fn run_with_timeout<F>(f: F, timeout: std::time::Duration) -> bool
where
    F: FnOnce() + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    rx.recv_timeout(timeout).is_ok()
}

impl Default for PluginManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spaph_plugin::{ParamInfo, PluginInfo as PI, PluginInstance};
    use spaph_types::PluginFormat;
    use std::path::PathBuf;

    struct MockPlugin {
        info: PI,
    }
    impl PluginInstance for MockPlugin {
        fn info(&self) -> &PI {
            &self.info
        }
        fn set_sample_rate(&mut self, _sr: f64) {}
        fn activate(&mut self) -> Result<()> {
            Ok(())
        }
        fn deactivate(&mut self) {}
        fn process(&mut self, _i: &[&[f32]], _o: &mut [&mut [f32]], _n: usize) {}
        fn get_param(&self, _index: u32) -> f64 {
            0.0
        }
        fn set_param(&mut self, _index: u32, _value: f64) {}
        fn save_state(&self) -> Result<Vec<u8>> {
            Ok(vec![])
        }
        fn load_state(&mut self, _data: &[u8]) -> Result<()> {
            Ok(())
        }
    }

    fn mock_info() -> PI {
        PI {
            name: "Mock".into(),
            vendor: "Test".into(),
            version: "1".into(),
            format: PluginFormat::Vst3,
            num_inputs: 2,
            num_outputs: 2,
            params: vec![ParamInfo {
                index: 0,
                name: "p".into(),
                default_value: 0.5,
                min: 0.0,
                max: 1.0,
            }],
            presets: vec!["Preset A".into(), "Preset B".into(), "Preset C".into()],
        }
    }

    // Helper to insert a mock directly
    fn insert_mock(mgr: &mut PluginManager) -> u32 {
        let id = mgr.next_id.fetch_add(1, Ordering::SeqCst);
        mgr.plugins.push(LoadedPlugin {
            id,
            alias: None,
            input_from: None,
            path: PathBuf::from("/mock"),
            info: mock_info(),
            instance: std::sync::Arc::new(std::sync::Mutex::new(Box::new(MockPlugin { info: mock_info() }))),
            midi_queue: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            gain: std::sync::Arc::new(std::sync::Mutex::new(1.0)),
            midi_connections: Vec::new(),
            audio_device: None,
        });
        id
    }

    #[test]
    fn empty_manager() {
        let mgr = PluginManager::new();
        assert!(mgr.list().is_empty());
        assert_eq!(mgr.count(), 0);
    }

    #[test]
    fn insert_and_list() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        assert_eq!(id, 1);
        let list = mgr.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, 1);
        assert_eq!(list[0].name, "Mock");
        assert_eq!(list[0].num_params, 1);
    }

    #[test]
    fn multiple_instances_get_distinct_ids() {
        let mut mgr = PluginManager::new();
        let a = insert_mock(&mut mgr);
        let b = insert_mock(&mut mgr);
        assert_ne!(a, b);
        assert_eq!(mgr.count(), 2);
    }

    #[test]
    fn unload_removes() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        assert!(mgr.unload(id).is_ok());
        assert!(mgr.list().is_empty());
    }

    #[test]
    fn unload_unknown_errors() {
        let mut mgr = PluginManager::new();
        assert!(mgr.unload(999).is_err());
    }

    #[test]
    fn alias_resolves_to_id() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        mgr.plugins[0].alias = Some("synth".into());
        assert_eq!(mgr.resolve("synth"), Some(id));
    }

    #[test]
    fn numeric_token_resolves_to_id_even_if_unknown() {
        let mgr = PluginManager::new();
        assert_eq!(mgr.resolve("7"), Some(7));
    }

    #[test]
    fn unknown_alias_does_not_resolve() {
        let mut mgr = PluginManager::new();
        insert_mock(&mut mgr);
        assert_eq!(mgr.resolve("nosuch"), None);
    }

    #[test]
    fn alias_is_case_sensitive() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        mgr.plugins[0].alias = Some("Synth".into());
        assert_eq!(mgr.resolve("Synth"), Some(id));
        assert_eq!(mgr.resolve("synth"), None);
    }

    #[test]
    fn validate_alias_accepts_plain_words() {
        for a in ["amsynth", "synth_1", "a-b", "_x", "Pad2"] {
            assert!(validate_alias(a).is_ok(), "{a}");
        }
    }

    #[test]
    fn validate_alias_rejects_bad_names() {
        for a in ["", "1synth", "-x", "all", "my synth", "a/b", "é", "a.b"] {
            assert!(validate_alias(a).is_err(), "{a:?} should be rejected");
        }
    }

    #[test]
    fn alias_released_after_unload() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        mgr.plugins[0].alias = Some("synth".into());
        mgr.unload(id).unwrap();
        assert_eq!(mgr.resolve("synth"), None);
        // The name is free again (a later load would not be a duplicate).
        assert!(mgr.plugins.iter().all(|p| p.alias.as_deref() != Some("synth")));
    }

    #[test]
    fn load_as_rejects_invalid_alias_before_loading() {
        let mut mgr = PluginManager::new();
        let err = mgr.load_as(Path::new("/no/such.vst3"), Some("1bad")).unwrap_err();
        assert!(err.to_string().contains("invalid alias"), "{err}");
    }

    #[test]
    fn load_as_rejects_duplicate_alias_before_loading() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        mgr.plugins[0].alias = Some("synth".into());
        let err = mgr.load_as(Path::new("/no/such.vst3"), Some("synth")).unwrap_err();
        assert!(err.to_string().contains("already in use"), "{err}");
        assert_eq!(mgr.resolve("synth"), Some(id));
    }

    fn insert_mock_inputs(mgr: &mut PluginManager, inputs: u32) -> u32 {
        let id = insert_mock(mgr);
        mgr.plugins.last_mut().unwrap().info.num_inputs = inputs;
        id
    }

    #[test]
    fn connect_sets_input_source() {
        let mut mgr = PluginManager::new();
        let a = insert_mock(&mut mgr);
        let b = insert_mock(&mut mgr);
        mgr.connect(a, b).unwrap();
        assert_eq!(mgr.input_source(b), Some(a));
        assert_eq!(mgr.links(), vec![(b, a)]);
    }

    #[test]
    fn connect_rejects_self_link() {
        let mut mgr = PluginManager::new();
        let a = insert_mock(&mut mgr);
        assert!(mgr.connect(a, a).is_err());
        assert!(mgr.links().is_empty());
    }

    #[test]
    fn connect_rejects_unknown_plugins() {
        let mut mgr = PluginManager::new();
        let a = insert_mock(&mut mgr);
        assert!(mgr.connect(999, a).unwrap_err().to_string().contains("no plugin with id 999"));
        assert!(mgr.connect(a, 999).is_err());
        assert!(mgr.links().is_empty());
    }

    #[test]
    fn connect_rejects_target_without_audio_inputs() {
        let mut mgr = PluginManager::new();
        let synth = insert_mock(&mut mgr);
        let instrument = insert_mock_inputs(&mut mgr, 0);
        let err = mgr.connect(synth, instrument).unwrap_err().to_string();
        assert!(err.contains("has no audio inputs"), "{err}");
        assert!(mgr.links().is_empty());
    }

    #[test]
    fn connect_rejects_cycle_without_changing_state() {
        let mut mgr = PluginManager::new();
        let a = insert_mock(&mut mgr);
        let b = insert_mock(&mut mgr);
        let c = insert_mock(&mut mgr);
        mgr.connect(a, b).unwrap();
        mgr.connect(b, c).unwrap();
        let err = mgr.connect(c, a).unwrap_err().to_string();
        assert!(err.contains("would create a cycle"), "{err}");
        assert_eq!(mgr.links(), vec![(b, a), (c, b)]);
    }

    #[test]
    fn fan_out_is_allowed() {
        let mut mgr = PluginManager::new();
        let a = insert_mock(&mut mgr);
        let b = insert_mock(&mut mgr);
        let c = insert_mock(&mut mgr);
        mgr.connect(a, b).unwrap();
        mgr.connect(a, c).unwrap();
        assert_eq!(mgr.input_source(b), Some(a));
        assert_eq!(mgr.input_source(c), Some(a));
    }

    #[test]
    fn connect_replaces_existing_source() {
        let mut mgr = PluginManager::new();
        let a = insert_mock(&mut mgr);
        let b = insert_mock(&mut mgr);
        let c = insert_mock(&mut mgr);
        mgr.connect(a, b).unwrap();
        mgr.connect(c, b).unwrap();
        assert_eq!(mgr.input_source(b), Some(c));
        assert_eq!(mgr.links(), vec![(b, c)]);
    }

    #[test]
    fn disconnect_removes_link() {
        let mut mgr = PluginManager::new();
        let a = insert_mock(&mut mgr);
        let b = insert_mock(&mut mgr);
        mgr.connect(a, b).unwrap();
        mgr.disconnect(b).unwrap();
        assert_eq!(mgr.input_source(b), None);
        assert!(mgr.links().is_empty());
    }

    #[test]
    fn disconnect_unlinked_plugin_errors() {
        let mut mgr = PluginManager::new();
        let b = insert_mock(&mut mgr);
        let err = mgr.disconnect(b).unwrap_err().to_string();
        assert!(err.contains("has no input connection"), "{err}");
    }

    #[test]
    fn disconnect_unknown_plugin_errors() {
        let mut mgr = PluginManager::new();
        assert!(mgr.disconnect(999).is_err());
    }

    #[test]
    fn unload_of_source_clears_downstream_link() {
        let mut mgr = PluginManager::new();
        let a = insert_mock(&mut mgr);
        let b = insert_mock(&mut mgr);
        mgr.connect(a, b).unwrap();
        mgr.unload(a).unwrap();
        assert_eq!(mgr.input_source(b), None);
        assert!(mgr.links().is_empty());
    }

    #[test]
    fn unload_of_target_removes_its_link() {
        let mut mgr = PluginManager::new();
        let a = insert_mock(&mut mgr);
        let b = insert_mock(&mut mgr);
        mgr.connect(a, b).unwrap();
        mgr.unload(b).unwrap();
        assert!(mgr.links().is_empty());
    }

    #[test]
    fn list_reports_input_source() {
        let mut mgr = PluginManager::new();
        let a = insert_mock(&mut mgr);
        let b = insert_mock(&mut mgr);
        mgr.connect(a, b).unwrap();
        let list = mgr.list();
        assert_eq!(list[0].input_from, None);
        assert_eq!(list[1].input_from, Some(a));
    }

    #[test]
    fn list_presets_returns_names() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        let ps = mgr.list_presets(id).unwrap();
        assert_eq!(ps.len(), 3);
        assert_eq!(ps[0], "Preset A");
    }

    #[test]
    fn list_presets_unknown_errors() {
        let mgr = PluginManager::new();
        assert!(mgr.list_presets(999).is_err());
    }

    #[test]
    fn select_preset_valid() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        assert!(mgr.select_preset(id, 0).is_ok());
    }

    #[test]
    fn select_preset_out_of_range() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        assert!(mgr.select_preset(id, 3).is_err());
    }

    #[test]
    fn select_preset_unknown_errors() {
        let mut mgr = PluginManager::new();
        assert!(mgr.select_preset(999, 0).is_err());
    }

    #[test]
    fn gain_defaults_to_one() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        assert_eq!(mgr.get_gain(id).unwrap(), 1.0);
    }

    #[test]
    fn set_gain_roundtrip() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        mgr.set_gain(id, 0.42).unwrap();
        assert!((mgr.get_gain(id).unwrap() - 0.42).abs() < 1e-6);
    }

    #[test]
    fn take_all_empties_manager() {
        let mut mgr = PluginManager::new();
        insert_mock(&mut mgr);
        insert_mock(&mut mgr);
        let taken = mgr.take_all();
        assert_eq!(taken.len(), 2);
        assert_eq!(mgr.count(), 0);
        assert!(mgr.list().is_empty());
    }

    #[test]
    fn run_with_timeout_finishes_in_time() {
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = done.clone();
        let ok = run_with_timeout(
            move || d.store(true, std::sync::atomic::Ordering::SeqCst),
            std::time::Duration::from_secs(5),
        );
        assert!(ok);
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn run_with_timeout_gives_up_on_hang() {
        let start = std::time::Instant::now();
        let ok = run_with_timeout(
            || std::thread::sleep(std::time::Duration::from_secs(30)),
            std::time::Duration::from_millis(200),
        );
        assert!(!ok);
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn gain_unknown_errors() {
        let mut mgr = PluginManager::new();
        assert!(mgr.get_gain(999).is_err());
        assert!(mgr.set_gain(999, 1.0).is_err());
    }

    #[test]
    fn midi_connection_allows_different_channels() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        assert!(mgr.add_midi_connection(id, "port".into(), Some(1)).is_ok());
        assert!(mgr.add_midi_connection(id, "port".into(), Some(2)).is_ok());
        assert_eq!(mgr.midi_connections(id).len(), 2);
    }

    #[test]
    fn midi_connection_rejects_duplicate_channel() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        assert!(mgr.add_midi_connection(id, "port".into(), Some(1)).is_ok());
        assert!(mgr.add_midi_connection(id, "port".into(), Some(1)).is_err());
    }

    #[test]
    fn midi_connection_all_conflicts_with_specific() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        assert!(mgr.add_midi_connection(id, "port".into(), None).is_ok());
        assert!(mgr.add_midi_connection(id, "port".into(), Some(1)).is_err());
    }

    #[test]
    fn midi_connection_specific_conflicts_with_all() {
        let mut mgr = PluginManager::new();
        let id = insert_mock(&mut mgr);
        assert!(mgr.add_midi_connection(id, "port".into(), Some(1)).is_ok());
        assert!(mgr.add_midi_connection(id, "port".into(), None).is_err());
    }

    #[test]
    fn midi_connection_same_port_different_plugins_ok() {
        let mut mgr = PluginManager::new();
        let a = insert_mock(&mut mgr);
        let b = insert_mock(&mut mgr);
        assert!(mgr.add_midi_connection(a, "port".into(), None).is_ok());
        assert!(mgr.add_midi_connection(b, "port".into(), None).is_ok());
    }
}
