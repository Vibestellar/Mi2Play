use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AirplayPlayer {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub capabilities: BTreeMap<String, String>,
    pub online: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VirtualDevice {
    pub id: String,
    pub name: String,
    pub player_id: String,
    pub state: PlaybackState,
    pub volume: u8,
    pub client_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackState {
    #[default]
    Idle,
    Playing,
    Paused,
    Stopped,
    Error,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RegistryError {
    #[error("player '{0}' was not found")]
    PlayerNotFound(String),
    #[error("virtual device '{0}' already exists")]
    DuplicateDevice(String),
    #[error("virtual device '{0}' was not found")]
    DeviceNotFound(String),
    #[error("player '{0}' is already bound")]
    PlayerAlreadyBound(String),
}

#[derive(Default)]
pub struct DeviceRegistry {
    players: BTreeMap<String, AirplayPlayer>,
    devices: BTreeMap<String, VirtualDevice>,
}

impl DeviceRegistry {
    pub fn upsert_player(&mut self, player: AirplayPlayer) {
        self.players.insert(player.id.clone(), player);
    }

    pub fn create_device(
        &mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        player_id: impl Into<String>,
    ) -> Result<&VirtualDevice, RegistryError> {
        let id = id.into();
        let player_id = player_id.into();
        if self.devices.contains_key(&id) {
            return Err(RegistryError::DuplicateDevice(id));
        }
        if !self.players.contains_key(&player_id) {
            return Err(RegistryError::PlayerNotFound(player_id));
        }
        if self
            .devices
            .values()
            .any(|device| device.player_id == player_id)
        {
            return Err(RegistryError::PlayerAlreadyBound(player_id));
        }
        self.devices.insert(
            id.clone(),
            VirtualDevice {
                id: id.clone(),
                name: name.into(),
                player_id,
                state: PlaybackState::Idle,
                volume: 100,
                client_id: None,
            },
        );
        Ok(self.devices.get(&id).expect("inserted device must exist"))
    }

    pub fn device(&self, id: &str) -> Option<&VirtualDevice> {
        self.devices.get(id)
    }

    pub fn device_mut(&mut self, id: &str) -> Result<&mut VirtualDevice, RegistryError> {
        self.devices
            .get_mut(id)
            .ok_or_else(|| RegistryError::DeviceNotFound(id.into()))
    }

    pub fn players(&self) -> impl Iterator<Item = &AirplayPlayer> {
        self.players.values()
    }

    pub fn devices(&self) -> impl Iterator<Item = &VirtualDevice> {
        self.devices.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(id: &str) -> AirplayPlayer {
        AirplayPlayer {
            id: id.into(),
            name: "speaker".into(),
            host: "192.0.2.1".into(),
            port: 7000,
            capabilities: BTreeMap::new(),
            online: true,
        }
    }

    #[test]
    fn maps_one_virtual_device_to_one_player() {
        let mut registry = DeviceRegistry::default();
        registry.upsert_player(player("p1"));
        registry.create_device("d1", "客厅", "p1").unwrap();
        assert_eq!(registry.device("d1").unwrap().player_id, "p1");
        assert_eq!(
            registry.create_device("d2", "重复", "p1"),
            Err(RegistryError::PlayerAlreadyBound("p1".into()))
        );
    }
}
