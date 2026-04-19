use std::collections::HashMap;
use std::sync::RwLock;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaDescriptor {
    pub event_type: String,
    pub version: u32,
    pub protobuf_descriptor: Vec<u8>,
}

pub struct SchemaRegistry {
    schemas: RwLock<HashMap<(String, u32), SchemaDescriptor>>,
    latest_versions: RwLock<HashMap<String, u32>>,
}

impl SchemaRegistry {
    pub fn new() -> Self {
        Self {
            schemas: RwLock::new(HashMap::new()),
            latest_versions: RwLock::new(HashMap::new()),
        }
    }

    pub fn register(&self, descriptor: SchemaDescriptor) -> Result<()> {
        let key = (descriptor.event_type.clone(), descriptor.version);
        let mut schemas = self.schemas.write().unwrap();
        if schemas.contains_key(&key) {
            anyhow::bail!(
                "Schema already registered: {} v{}",
                descriptor.event_type,
                descriptor.version
            );
        }

        let event_type = descriptor.event_type.clone();
        let version = descriptor.version;
        schemas.insert(key, descriptor);

        let mut latest = self.latest_versions.write().unwrap();
        let current_latest = latest.get(&event_type).copied().unwrap_or(0);
        if version > current_latest {
            latest.insert(event_type, version);
        }

        Ok(())
    }

    pub fn get(&self, event_type: &str, version: u32) -> Option<SchemaDescriptor> {
        self.schemas
            .read()
            .unwrap()
            .get(&(event_type.to_string(), version))
            .cloned()
    }

    pub fn latest_version(&self, event_type: &str) -> Option<u32> {
        self.latest_versions
            .read()
            .unwrap()
            .get(event_type)
            .copied()
    }

    pub fn is_registered(&self, event_type: &str, version: u32) -> bool {
        self.schemas
            .read()
            .unwrap()
            .contains_key(&(event_type.to_string(), version))
    }

    pub fn versions(&self, event_type: &str) -> Vec<u32> {
        let schemas = self.schemas.read().unwrap();
        let mut versions: Vec<u32> = schemas
            .keys()
            .filter(|(et, _)| et == event_type)
            .map(|(_, v)| *v)
            .collect();
        versions.sort();
        versions
    }
}

impl Default for SchemaRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub type UpcasterFn = Box<dyn Fn(&[u8], u32) -> Result<Vec<u8>> + Send + Sync>;

pub struct Upcaster {
    upcasters: RwLock<HashMap<(String, u32, u32), UpcasterFn>>,
}

impl Upcaster {
    pub fn new() -> Self {
        Self {
            upcasters: RwLock::new(HashMap::new()),
        }
    }

    pub fn register(
        &self,
        event_type: &str,
        from_version: u32,
        to_version: u32,
        upcaster: UpcasterFn,
    ) {
        self.upcasters
            .write()
            .unwrap()
            .insert((event_type.to_string(), from_version, to_version), upcaster);
    }

    pub fn upcast(
        &self,
        event_type: &str,
        from_version: u32,
        to_version: u32,
        data: &[u8],
    ) -> Result<Vec<u8>> {
        let upcasters = self.upcasters.read().unwrap();
        let key = (event_type.to_string(), from_version, to_version);
        let upcaster = upcasters.get(&key).with_context(|| {
            format!(
                "No upcaster registered: {} v{} -> v{}",
                event_type, from_version, to_version
            )
        })?;
        upcaster(data, from_version)
    }

    pub fn upcast_to_latest(
        &self,
        event_type: &str,
        current_version: u32,
        data: &[u8],
        registry: &SchemaRegistry,
    ) -> Result<Vec<u8>> {
        let latest = registry
            .latest_version(event_type)
            .context("No schema registered for event type")?;

        if current_version == latest {
            return Ok(data.to_vec());
        }

        let mut data = data.to_vec();
        let mut version = current_version;

        while version < latest {
            let next = version + 1;
            data = self.upcast(event_type, version, next, &data)?;
            version = next;
        }

        Ok(data)
    }
}

impl Default for Upcaster {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_and_get() {
        let registry = SchemaRegistry::new();
        let descriptor = SchemaDescriptor {
            event_type: "DecisionEvent".to_string(),
            version: 1,
            protobuf_descriptor: vec![1, 2, 3],
        };

        registry.register(descriptor).unwrap();

        let retrieved = registry.get("DecisionEvent", 1).unwrap();
        assert_eq!(retrieved.event_type, "DecisionEvent");
        assert_eq!(retrieved.version, 1);
    }

    #[test]
    fn test_register_duplicate_fails() {
        let registry = SchemaRegistry::new();
        let d1 = SchemaDescriptor {
            event_type: "TestEvent".to_string(),
            version: 1,
            protobuf_descriptor: vec![],
        };
        let d2 = SchemaDescriptor {
            event_type: "TestEvent".to_string(),
            version: 1,
            protobuf_descriptor: vec![1],
        };

        registry.register(d1).unwrap();
        assert!(registry.register(d2).is_err());
    }

    #[test]
    fn test_latest_version() {
        let registry = SchemaRegistry::new();
        for v in 1..=3 {
            registry
                .register(SchemaDescriptor {
                    event_type: "ExecEvent".to_string(),
                    version: v,
                    protobuf_descriptor: vec![v as u8],
                })
                .unwrap();
        }

        assert_eq!(registry.latest_version("ExecEvent"), Some(3));
        assert_eq!(registry.latest_version("Unknown"), None);
    }

    #[test]
    fn test_versions_list() {
        let registry = SchemaRegistry::new();
        for v in [3, 1, 2] {
            registry
                .register(SchemaDescriptor {
                    event_type: "OrderEvent".to_string(),
                    version: v,
                    protobuf_descriptor: vec![],
                })
                .unwrap();
        }

        assert_eq!(registry.versions("OrderEvent"), vec![1, 2, 3]);
    }

    #[test]
    fn test_upcast_single_step() {
        let upcaster = Upcaster::new();
        upcaster.register(
            "TestEvent",
            1,
            2,
            Box::new(|data: &[u8], _from: u32| {
                let mut result = data.to_vec();
                result.push(0xFF);
                Ok(result)
            }),
        );

        let result = upcaster.upcast("TestEvent", 1, 2, b"v1").unwrap();
        assert_eq!(result, b"v1\xff");
    }

    #[test]
    fn test_upcast_chain_to_latest() {
        let registry = SchemaRegistry::new();
        for v in 1..=3 {
            registry
                .register(SchemaDescriptor {
                    event_type: "ChainEvent".to_string(),
                    version: v,
                    protobuf_descriptor: vec![],
                })
                .unwrap();
        }

        let upcaster = Upcaster::new();
        upcaster.register(
            "ChainEvent",
            1,
            2,
            Box::new(|data: &[u8], _from: u32| {
                let mut r = data.to_vec();
                r.push(b'A');
                Ok(r)
            }),
        );
        upcaster.register(
            "ChainEvent",
            2,
            3,
            Box::new(|data: &[u8], _from: u32| {
                let mut r = data.to_vec();
                r.push(b'B');
                Ok(r)
            }),
        );

        let result = upcaster
            .upcast_to_latest("ChainEvent", 1, b"base", &registry)
            .unwrap();
        assert_eq!(result, b"baseAB");
    }

    #[test]
    fn test_upcast_already_latest() {
        let registry = SchemaRegistry::new();
        registry
            .register(SchemaDescriptor {
                event_type: "Noop".to_string(),
                version: 2,
                protobuf_descriptor: vec![],
            })
            .unwrap();

        let upcaster = Upcaster::new();
        let result = upcaster
            .upcast_to_latest("Noop", 2, b"unchanged", &registry)
            .unwrap();
        assert_eq!(result, b"unchanged");
    }
}
