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
    pub name: String,
    pub vendor: String,
    pub format: PluginFormat,
    pub num_inputs: u32,
    pub num_outputs: u32,
    pub num_params: usize,
    pub path: PathBuf,
}

pub struct LoadedPlugin {
    pub id: u32,
    pub path: PathBuf,
    pub info: SpaphPluginInfo,
    pub instance: std::sync::Arc<std::sync::Mutex<Box<dyn PluginInstance>>>,
    pub midi_queue: std::sync::Arc<std::sync::Mutex<Vec<spaph_types::MidiEvent>>>,
    pub gain: std::sync::Arc<std::sync::Mutex<f32>>,
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
            path: path.to_path_buf(),
            info,
            instance: std::sync::Arc::new(std::sync::Mutex::new(instance)),
            midi_queue: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            gain: std::sync::Arc::new(std::sync::Mutex::new(1.0)),
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
        Ok(())
    }

    pub fn list(&self) -> Vec<PluginSummary> {
        self.plugins
            .iter()
            .map(|p| PluginSummary {
                id: p.id,
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

    pub fn count(&self) -> usize {
        self.plugins.len()
    }
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
            path: PathBuf::from("/mock"),
            info: mock_info(),
            instance: std::sync::Arc::new(std::sync::Mutex::new(Box::new(MockPlugin { info: mock_info() }))),
            midi_queue: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            gain: std::sync::Arc::new(std::sync::Mutex::new(1.0)),
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
    fn gain_unknown_errors() {
        let mut mgr = PluginManager::new();
        assert!(mgr.get_gain(999).is_err());
        assert!(mgr.set_gain(999, 1.0).is_err());
    }
}
