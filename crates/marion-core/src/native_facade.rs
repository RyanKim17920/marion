use std::collections::HashMap;

/// The exact command words owned by marion rather than a native facade.
const RESERVED_COMMANDS: &[&str] = &["help", "doctor", "version", "run", "attach", "tree", "mcp"];

/// A named native facade. This is metadata only; it neither locates nor launches a program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFacadeDescriptor {
    pub command: &'static str,
    pub aliases: &'static [&'static str],
    pub agent_type: &'static str,
    pub readiness: NativeFacadeReadiness,
}

/// Whether a descriptor is eligible for launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeFacadeReadiness {
    Planned,
    Ready,
}

/// The reason a command token cannot be registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NativeFacadeTokenError {
    #[error("is empty")]
    Empty,
    #[error("contains non-ASCII characters")]
    NonAscii,
    #[error("starts with '-'")]
    LeadingDash,
}

/// A malformed or ambiguous native-facade descriptor set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NativeFacadeValidationError {
    #[error("invalid native facade command {command:?}: {reason}")]
    InvalidCommand {
        command: &'static str,
        reason: NativeFacadeTokenError,
    },
    #[error("invalid alias {alias:?} for native facade command {command:?}: {reason}")]
    InvalidAlias {
        command: &'static str,
        alias: &'static str,
        reason: NativeFacadeTokenError,
    },
    #[error("native facade command {command:?} is reserved")]
    ReservedCommand { command: &'static str },
    #[error("native facade alias {alias:?} for {command:?} is reserved")]
    ReservedAlias {
        command: &'static str,
        alias: &'static str,
    },
    #[error("native facade command {command:?} is registered more than once")]
    DuplicatePrimary { command: &'static str },
    #[error(
        "native facade alias {alias:?} for command {alias_command:?} collides with primary command {primary_command:?}"
    )]
    AliasMatchesPrimary {
        alias: &'static str,
        alias_command: &'static str,
        primary_command: &'static str,
    },
    #[error(
        "native facade alias {alias:?} is registered for both {first_command:?} and {second_command:?}"
    )]
    DuplicateAlias {
        alias: &'static str,
        first_command: &'static str,
        second_command: &'static str,
    },
}

/// A validated borrowed descriptor collection.
///
/// Resolution is deliberately a name lookup over these descriptors only. In particular, it never
/// reads `PATH`, probes the filesystem, installs a program, or derives a program path.
#[derive(Debug)]
pub struct NativeFacadeRegistry<'a> {
    descriptors: &'a [NativeFacadeDescriptor],
}

impl<'a> NativeFacadeRegistry<'a> {
    /// Validates every spelling in one registration pass before exposing the descriptors.
    pub fn new(
        descriptors: &'a [NativeFacadeDescriptor],
    ) -> Result<Self, NativeFacadeValidationError> {
        let mut primaries = HashMap::with_capacity(descriptors.len());
        let alias_capacity = descriptors
            .iter()
            .map(|descriptor| descriptor.aliases.len())
            .sum();
        let mut aliases = HashMap::with_capacity(alias_capacity);

        for descriptor in descriptors {
            validate_command(descriptor.command)?;
            if RESERVED_COMMANDS.contains(&descriptor.command) {
                return Err(NativeFacadeValidationError::ReservedCommand {
                    command: descriptor.command,
                });
            }
            if primaries.insert(descriptor.command, ()).is_some() {
                return Err(NativeFacadeValidationError::DuplicatePrimary {
                    command: descriptor.command,
                });
            }
            if let Some(&alias_command) = aliases.get(descriptor.command) {
                return Err(NativeFacadeValidationError::AliasMatchesPrimary {
                    alias: descriptor.command,
                    alias_command,
                    primary_command: descriptor.command,
                });
            }

            for &alias in descriptor.aliases {
                validate_alias(descriptor.command, alias)?;
                if RESERVED_COMMANDS.contains(&alias) {
                    return Err(NativeFacadeValidationError::ReservedAlias {
                        command: descriptor.command,
                        alias,
                    });
                }
                if primaries.contains_key(alias) {
                    return Err(NativeFacadeValidationError::AliasMatchesPrimary {
                        alias,
                        alias_command: descriptor.command,
                        primary_command: alias,
                    });
                }
                if let Some(first_command) = aliases.insert(alias, descriptor.command) {
                    return Err(NativeFacadeValidationError::DuplicateAlias {
                        alias,
                        first_command,
                        second_command: descriptor.command,
                    });
                }
            }
        }

        Ok(Self { descriptors })
    }

    /// Resolves a primary command or alias for diagnostics, including planned descriptors.
    pub fn resolve(&self, spelling: &str) -> Option<&'a NativeFacadeDescriptor> {
        self.descriptors.iter().find(|descriptor| {
            descriptor.command == spelling || descriptor.aliases.contains(&spelling)
        })
    }

    /// Resolves only facades that are currently permitted to launch.
    pub fn resolve_for_launch(&self, spelling: &str) -> Option<&'a NativeFacadeDescriptor> {
        self.resolve(spelling)
            .filter(|descriptor| descriptor.readiness == NativeFacadeReadiness::Ready)
    }

    /// Primary command names that are currently permitted to launch.
    pub fn ready_commands(&self) -> Vec<&'a str> {
        self.descriptors
            .iter()
            .filter(|descriptor| descriptor.readiness == NativeFacadeReadiness::Ready)
            .map(|descriptor| descriptor.command)
            .collect()
    }
}

fn validate_command(command: &'static str) -> Result<(), NativeFacadeValidationError> {
    token_error(command)
        .map_err(|reason| NativeFacadeValidationError::InvalidCommand { command, reason })
}

fn validate_alias(
    command: &'static str,
    alias: &'static str,
) -> Result<(), NativeFacadeValidationError> {
    token_error(alias).map_err(|reason| NativeFacadeValidationError::InvalidAlias {
        command,
        alias,
        reason,
    })
}

fn token_error(token: &str) -> Result<(), NativeFacadeTokenError> {
    if token.is_empty() {
        Err(NativeFacadeTokenError::Empty)
    } else if !token.is_ascii() {
        Err(NativeFacadeTokenError::NonAscii)
    } else if token.starts_with('-') {
        Err(NativeFacadeTokenError::LeadingDash)
    } else {
        Ok(())
    }
}

/// Production intentionally advertises no native facade until one is independently implemented.
pub const PRODUCTION_NATIVE_FACADES: &[NativeFacadeDescriptor] = &[];

/// The production registry gate. Its empty descriptor set permits no native-facade launch.
pub fn production_native_facades() -> NativeFacadeRegistry<'static> {
    NativeFacadeRegistry::new(PRODUCTION_NATIVE_FACADES)
        .expect("the built-in native facade descriptor slice is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATLAS: NativeFacadeDescriptor = NativeFacadeDescriptor {
        command: "atlas",
        aliases: &["at"],
        agent_type: "atlas-agent",
        readiness: NativeFacadeReadiness::Ready,
    };
    const BOREAL: NativeFacadeDescriptor = NativeFacadeDescriptor {
        command: "boreal",
        aliases: &["bo"],
        agent_type: "boreal-agent",
        readiness: NativeFacadeReadiness::Planned,
    };

    #[test]
    fn resolves_an_exact_primary_name() {
        let registry = NativeFacadeRegistry::new(&[ATLAS]).unwrap();

        assert_eq!(registry.resolve("atlas"), Some(&ATLAS));
    }

    #[test]
    fn resolves_an_explicit_alias() {
        let registry = NativeFacadeRegistry::new(&[ATLAS]).unwrap();

        assert_eq!(registry.resolve("at"), Some(&ATLAS));
    }

    #[test]
    fn lookup_is_case_sensitive() {
        let registry = NativeFacadeRegistry::new(&[ATLAS]).unwrap();

        assert_eq!(registry.resolve("Atlas"), None);
        assert_eq!(registry.resolve("AT"), None);
    }

    #[test]
    fn unknown_input_is_not_discovered_from_path() {
        let registry = NativeFacadeRegistry::new(&[ATLAS]).unwrap();

        // `sh` is an executable on the test host's PATH, but a pure registry only resolves names
        // explicitly registered in its descriptors.
        assert_eq!(registry.resolve("sh"), None);
    }

    #[test]
    fn rejects_a_reserved_primary_name() {
        let descriptor = NativeFacadeDescriptor {
            command: "help",
            ..ATLAS
        };

        assert_eq!(
            NativeFacadeRegistry::new(&[descriptor]).unwrap_err(),
            NativeFacadeValidationError::ReservedCommand { command: "help" }
        );
    }

    #[test]
    fn rejects_a_reserved_alias() {
        let descriptor = NativeFacadeDescriptor {
            aliases: &["doctor"],
            ..ATLAS
        };

        assert_eq!(
            NativeFacadeRegistry::new(&[descriptor]).unwrap_err(),
            NativeFacadeValidationError::ReservedAlias {
                command: "atlas",
                alias: "doctor",
            }
        );
    }

    #[test]
    fn rejects_each_omitted_control_verb_as_a_primary_and_alias() {
        for (verb, aliases) in [
            ("run", &["run"] as &[_]),
            ("attach", &["attach"]),
            ("tree", &["tree"]),
            ("mcp", &["mcp"]),
        ] {
            let primary = NativeFacadeDescriptor {
                command: verb,
                ..ATLAS
            };
            assert_eq!(
                NativeFacadeRegistry::new(&[primary]).unwrap_err(),
                NativeFacadeValidationError::ReservedCommand { command: verb }
            );

            let alias = NativeFacadeDescriptor { aliases, ..ATLAS };
            assert_eq!(
                NativeFacadeRegistry::new(&[alias]).unwrap_err(),
                NativeFacadeValidationError::ReservedAlias {
                    command: "atlas",
                    alias: verb,
                }
            );
        }
    }

    #[test]
    fn rejects_duplicate_primary_names() {
        let duplicate = NativeFacadeDescriptor {
            aliases: &["atlas-two"],
            ..ATLAS
        };

        assert_eq!(
            NativeFacadeRegistry::new(&[ATLAS, duplicate]).unwrap_err(),
            NativeFacadeValidationError::DuplicatePrimary { command: "atlas" }
        );
    }

    #[test]
    fn a_primary_alias_collision_names_both_owners_in_either_registration_order() {
        let alias_owner = NativeFacadeDescriptor {
            aliases: &["boreal"],
            ..ATLAS
        };

        for descriptors in [[alias_owner, BOREAL], [BOREAL, alias_owner]] {
            assert_eq!(
                NativeFacadeRegistry::new(&descriptors).unwrap_err(),
                NativeFacadeValidationError::AliasMatchesPrimary {
                    alias: "boreal",
                    alias_command: "atlas",
                    primary_command: "boreal",
                }
            );
        }
    }

    #[test]
    fn rejects_an_alias_that_collides_with_another_alias() {
        let duplicate_alias = NativeFacadeDescriptor {
            command: "cinder",
            aliases: &["at"],
            agent_type: "cinder-agent",
            readiness: NativeFacadeReadiness::Ready,
        };

        assert_eq!(
            NativeFacadeRegistry::new(&[ATLAS, duplicate_alias]).unwrap_err(),
            NativeFacadeValidationError::DuplicateAlias {
                alias: "at",
                first_command: "atlas",
                second_command: "cinder",
            }
        );
    }

    #[test]
    fn rejects_invalid_command_and_alias_tokens() {
        for (descriptor, error) in [
            (
                NativeFacadeDescriptor {
                    command: "",
                    ..ATLAS
                },
                NativeFacadeValidationError::InvalidCommand {
                    command: "",
                    reason: NativeFacadeTokenError::Empty,
                },
            ),
            (
                NativeFacadeDescriptor {
                    command: "météore",
                    ..ATLAS
                },
                NativeFacadeValidationError::InvalidCommand {
                    command: "météore",
                    reason: NativeFacadeTokenError::NonAscii,
                },
            ),
            (
                NativeFacadeDescriptor {
                    command: "-atlas",
                    ..ATLAS
                },
                NativeFacadeValidationError::InvalidCommand {
                    command: "-atlas",
                    reason: NativeFacadeTokenError::LeadingDash,
                },
            ),
            (
                NativeFacadeDescriptor {
                    aliases: &["-at"],
                    ..ATLAS
                },
                NativeFacadeValidationError::InvalidAlias {
                    command: "atlas",
                    alias: "-at",
                    reason: NativeFacadeTokenError::LeadingDash,
                },
            ),
        ] {
            assert_eq!(NativeFacadeRegistry::new(&[descriptor]).unwrap_err(), error);
        }
    }

    #[test]
    fn planned_descriptors_are_diagnostic_only() {
        let registry = NativeFacadeRegistry::new(&[ATLAS, BOREAL]).unwrap();

        assert_eq!(registry.resolve("boreal"), Some(&BOREAL));
        assert_eq!(registry.resolve_for_launch("boreal"), None);
        assert_eq!(registry.resolve_for_launch("bo"), None);
        assert_eq!(registry.resolve_for_launch("atlas"), Some(&ATLAS));
        assert_eq!(registry.ready_commands(), vec!["atlas"]);
    }

    #[test]
    fn production_registry_has_no_ready_native_facades() {
        assert!(production_native_facades().ready_commands().is_empty());
    }
}
