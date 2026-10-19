//! Pure reconciliation policy. Effects must reobserve after each action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Observation {
    Absent,
    Starting,
    Ready,
    Unhealthy {
        owned: bool,
        restart_blocker: Option<String>,
    },
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Start,
    Wait,
    Attach,
    FenceAndRestart,
    Refuse(String),
}

pub(crate) fn plan(observed: Observation, repaired: bool) -> Action {
    match observed {
        Observation::Absent => Action::Start,
        Observation::Starting => Action::Wait,
        Observation::Ready => Action::Attach,
        Observation::Unknown(reason) => Action::Refuse(reason),
        Observation::Unhealthy { owned: false, .. } => {
            Action::Refuse("selected runtime ownership is unproved; refusing restart".into())
        }
        Observation::Unhealthy {
            restart_blocker: Some(reason),
            ..
        } => Action::Refuse(reason),
        Observation::Unhealthy { .. } if repaired => {
            Action::Refuse("runtime remains unhealthy after one recovery attempt".into())
        }
        Observation::Unhealthy { .. } => Action::FenceAndRestart,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconciliation_converges_without_unsafe_or_repeated_restart() {
        assert_eq!(plan(Observation::Absent, false), Action::Start);
        assert_eq!(plan(Observation::Starting, false), Action::Wait);
        for repaired in [false, true] {
            assert_eq!(plan(Observation::Ready, repaired), Action::Attach);
            assert!(matches!(
                plan(
                    Observation::Unknown("authentication failed".into()),
                    repaired
                ),
                Action::Refuse(_)
            ));
            assert!(matches!(
                plan(
                    Observation::Unhealthy {
                        owned: false,
                        restart_blocker: None
                    },
                    repaired
                ),
                Action::Refuse(_)
            ));
            assert!(matches!(
                plan(
                    Observation::Unhealthy {
                        owned: true,
                        restart_blocker: Some("active executions".into())
                    },
                    repaired
                ),
                Action::Refuse(_)
            ));
        }
        let unhealthy = Observation::Unhealthy {
            owned: true,
            restart_blocker: None,
        };
        assert_eq!(plan(unhealthy.clone(), false), Action::FenceAndRestart);
        assert!(matches!(plan(unhealthy, true), Action::Refuse(_)));
    }
}
