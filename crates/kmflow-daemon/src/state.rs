use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    Disconnected,
    Discovering,
    Pairing,
    Connected,
    Active,
    Suspended,
    Reconnecting,
}

impl ConnectionState {
    pub fn can_transition_to(&self, next: ConnectionState) -> bool {
        use ConnectionState::*;
        matches!(
            (self, next),
            (Disconnected, Discovering)
                | (Discovering, Pairing)
                | (Discovering, Connected)
                | (Pairing, Connected)
                | (Connected, Active)
                | (Active, Suspended)
                | (Active, Disconnected)
                | (Suspended, Reconnecting)
                | (Reconnecting, Active)
                | (Reconnecting, Disconnected)
                // Allow direct transitions needed by current daemon flow
                | (Disconnected, Active)
                | (Active, Reconnecting)
                | (Connected, Disconnected)
        )
    }

    /// Validate and perform the transition. Returns the new state or an error.
    pub fn transition_to(&mut self, next: ConnectionState) -> anyhow::Result<()> {
        if self.can_transition_to(next) {
            tracing::debug!(from = %self, to = %next, "state transition");
            *self = next;
            Ok(())
        } else {
            anyhow::bail!("invalid state transition: {} -> {}", self, next)
        }
    }

    pub fn is_connected(&self) -> bool {
        matches!(
            self,
            ConnectionState::Connected
                | ConnectionState::Active
                | ConnectionState::Suspended
                | ConnectionState::Reconnecting
        )
    }
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected => write!(f, "disconnected"),
            Self::Discovering => write!(f, "discovering"),
            Self::Pairing => write!(f, "pairing"),
            Self::Connected => write!(f, "connected"),
            Self::Active => write!(f, "active"),
            Self::Suspended => write!(f, "suspended"),
            Self::Reconnecting => write!(f, "reconnecting"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_transitions() {
        let valid = vec![
            (ConnectionState::Disconnected, ConnectionState::Discovering),
            (ConnectionState::Disconnected, ConnectionState::Active),
            (ConnectionState::Discovering, ConnectionState::Connected),
            (ConnectionState::Connected, ConnectionState::Active),
            (ConnectionState::Active, ConnectionState::Disconnected),
            (ConnectionState::Active, ConnectionState::Reconnecting),
            (ConnectionState::Reconnecting, ConnectionState::Active),
            (ConnectionState::Reconnecting, ConnectionState::Disconnected),
        ];
        for (from, to) in valid {
            let mut state = from;
            assert!(state.transition_to(to).is_ok(), "{from} -> {to} 应该合法");
            assert_eq!(state, to);
        }
    }

    #[test]
    fn invalid_transitions() {
        let invalid = vec![
            (ConnectionState::Disconnected, ConnectionState::Reconnecting),
            (ConnectionState::Active, ConnectionState::Discovering),
            (ConnectionState::Connected, ConnectionState::Reconnecting),
            (ConnectionState::Reconnecting, ConnectionState::Pairing),
        ];
        for (from, to) in invalid {
            let mut state = from;
            assert!(state.transition_to(to).is_err(), "{from} -> {to} 应该非法");
            // 状态不变
            assert_eq!(state, from);
        }
    }

    #[test]
    fn is_connected_variants() {
        assert!(!ConnectionState::Disconnected.is_connected());
        assert!(!ConnectionState::Discovering.is_connected());
        assert!(ConnectionState::Connected.is_connected());
        assert!(ConnectionState::Active.is_connected());
        assert!(ConnectionState::Suspended.is_connected());
        assert!(ConnectionState::Reconnecting.is_connected());
    }

    #[test]
    fn display_format() {
        assert_eq!(ConnectionState::Active.to_string(), "active");
        assert_eq!(ConnectionState::Disconnected.to_string(), "disconnected");
        assert_eq!(ConnectionState::Reconnecting.to_string(), "reconnecting");
    }
}
