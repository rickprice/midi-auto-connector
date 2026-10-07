//! Pure rule-matching logic: given the current set of known ports and the
//! configured rules, compute which (source, dest) pairs should be linked.
//!
//! Deliberately backend-agnostic and free of any I/O, so it can be
//! exhaustively unit tested without touching ALSA or PipeWire.

use crate::config::Rule;
use crate::port::{PortId, PortInfo};

/// A single desired connection produced by matching the current port set
/// against the configured rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredConnection {
    pub rule_name: String,
    pub source: PortId,
    pub dest: PortId,
}

/// Compute every (source, dest) pair that should be connected given the
/// current set of known ports and the configured rules.
///
/// A connection is only ever proposed between two ports on the *same*
/// backend: ALSA-seq subscriptions and PipeWire links are different
/// mechanisms and can't bridge across each other. Each rule's `backend`
/// field further restricts which backend(s) it is evaluated against.
///
/// Pure and deterministic: the same `ports` and `rules` always produce the
/// same result, in rule order, then source order, then dest order (as
/// given in `ports`).
pub fn compute_desired_connections(ports: &[PortInfo], rules: &[Rule]) -> Vec<DesiredConnection> {
    let mut out = Vec::new();
    for rule in rules {
        for source in ports
            .iter()
            .filter(|p| rule.backend.applies_to(p.backend()))
            .filter(|p| p.direction.can_be_source)
            .filter(|p| rule.left.is_match(&p.full_name()))
        {
            for dest in ports
                .iter()
                .filter(|p| p.backend() == source.backend())
                .filter(|p| p.direction.can_be_sink)
                .filter(|p| p.id != source.id)
                .filter(|p| rule.right.is_match(&p.full_name()))
            {
                out.push(DesiredConnection {
                    rule_name: rule.name.clone(),
                    source: source.id.clone(),
                    dest: dest.id.clone(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuleBackend;
    use crate::port::PortDirection;
    use regex::Regex;

    fn rule(name: &str, backend: RuleBackend, left: &str, right: &str) -> Rule {
        Rule {
            name: name.to_string(),
            backend,
            left: Regex::new(left).unwrap(),
            right: Regex::new(right).unwrap(),
            on_connect: None,
            on_disconnect: None,
        }
    }

    fn alsa_port(
        client: i32,
        port: i32,
        client_name: &str,
        port_name: &str,
        src: bool,
        sink: bool,
    ) -> PortInfo {
        PortInfo {
            id: PortId::Alsa { client, port },
            client_name: client_name.to_string(),
            port_name: port_name.to_string(),
            direction: PortDirection {
                can_be_source: src,
                can_be_sink: sink,
            },
        }
    }

    fn pw_port(
        node_id: u32,
        port_id: u32,
        client_name: &str,
        port_name: &str,
        src: bool,
        sink: bool,
    ) -> PortInfo {
        PortInfo {
            id: PortId::PipeWire { node_id, port_id },
            client_name: client_name.to_string(),
            port_name: port_name.to_string(),
            direction: PortDirection {
                can_be_source: src,
                can_be_sink: sink,
            },
        }
    }

    #[test]
    fn matches_single_source_to_single_dest() {
        let ports = vec![
            alsa_port(128, 0, "Arturia KeyLab", "MIDI 1", true, false),
            alsa_port(129, 0, "FluidSynth", "MIDI In", false, true),
        ];
        let rules = vec![rule("r1", RuleBackend::Alsa, "^Arturia.*", "^FluidSynth.*")];

        let got = compute_desired_connections(&ports, &rules);
        assert_eq!(
            got,
            vec![DesiredConnection {
                rule_name: "r1".into(),
                source: ports[0].id.clone(),
                dest: ports[1].id.clone(),
            }]
        );
    }

    #[test]
    fn multiple_rule_rows_each_apply_independently() {
        let ports = vec![
            alsa_port(1, 0, "KeyboardA", "out", true, false),
            alsa_port(2, 0, "SynthA", "in", false, true),
            alsa_port(3, 0, "KeyboardB", "out", true, false),
            alsa_port(4, 0, "SynthB", "in", false, true),
        ];
        let rules = vec![
            rule("a-to-a", RuleBackend::Alsa, "^KeyboardA", "^SynthA"),
            rule("b-to-b", RuleBackend::Alsa, "^KeyboardB", "^SynthB"),
        ];

        let got = compute_desired_connections(&ports, &rules);
        assert_eq!(got.len(), 2);
        assert!(
            got.iter().any(|c| c.rule_name == "a-to-a"
                && c.source == ports[0].id
                && c.dest == ports[1].id)
        );
        assert!(
            got.iter().any(|c| c.rule_name == "b-to-b"
                && c.source == ports[2].id
                && c.dest == ports[3].id)
        );
    }

    #[test]
    fn fans_out_one_to_many_and_many_to_one() {
        let ports = vec![
            alsa_port(1, 0, "Keyboard", "out", true, false),
            alsa_port(2, 0, "SynthA", "in", false, true),
            alsa_port(3, 0, "SynthB", "in", false, true),
        ];
        let rules = vec![rule("fanout", RuleBackend::Alsa, "^Keyboard", "^Synth.*")];

        let got = compute_desired_connections(&ports, &rules);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn does_not_match_port_against_itself() {
        let ports = vec![alsa_port(1, 0, "Loopback", "port", true, true)];
        let rules = vec![rule("self", RuleBackend::Alsa, "^Loopback", "^Loopback")];

        let got = compute_desired_connections(&ports, &rules);
        assert!(got.is_empty());
    }

    #[test]
    fn respects_source_and_sink_capability_flags() {
        let ports = vec![
            // Can be neither a source nor a sink.
            alsa_port(1, 0, "NoCapabilities", "p", false, false),
            // Can be a source, but not a sink, so there is no valid sink
            // anywhere in the port set for it to pair with.
            alsa_port(2, 0, "SourceOnly", "p", true, false),
        ];
        let rules = vec![rule("r", RuleBackend::Alsa, ".*", ".*")];

        let got = compute_desired_connections(&ports, &rules);
        assert!(got.is_empty());
    }

    #[test]
    fn never_crosses_backends_even_with_matching_names() {
        let ports = vec![
            alsa_port(1, 0, "Device", "out", true, false),
            pw_port(10, 0, "Device", "in", false, true),
        ];
        let rules = vec![rule("cross", RuleBackend::Any, "^Device", "^Device")];

        let got = compute_desired_connections(&ports, &rules);
        assert!(
            got.is_empty(),
            "ALSA and PipeWire ports must never be paired together"
        );
    }

    #[test]
    fn rule_backend_alsa_ignores_pipewire_ports() {
        let ports = vec![
            pw_port(10, 0, "Device", "out", true, false),
            pw_port(11, 0, "Device", "in", false, true),
        ];
        let rules = vec![rule("alsa-only", RuleBackend::Alsa, "^Device", "^Device")];

        let got = compute_desired_connections(&ports, &rules);
        assert!(got.is_empty());
    }

    #[test]
    fn rule_backend_any_matches_within_each_backend_separately() {
        let ports = vec![
            alsa_port(1, 0, "Out", "p", true, false),
            alsa_port(2, 0, "In", "p", false, true),
            pw_port(10, 0, "Out", "p", true, false),
            pw_port(11, 0, "In", "p", false, true),
        ];
        let rules = vec![rule("any", RuleBackend::Any, "^Out", "^In")];

        let got = compute_desired_connections(&ports, &rules);
        assert_eq!(got.len(), 2);
        assert!(
            got.iter()
                .any(|c| c.source == ports[0].id && c.dest == ports[1].id)
        );
        assert!(
            got.iter()
                .any(|c| c.source == ports[2].id && c.dest == ports[3].id)
        );
    }

    #[test]
    fn no_rules_means_no_connections() {
        let ports = vec![
            alsa_port(1, 0, "A", "p", true, false),
            alsa_port(2, 0, "B", "p", false, true),
        ];
        assert!(compute_desired_connections(&ports, &[]).is_empty());
    }

    #[test]
    fn no_ports_means_no_connections() {
        let rules = vec![rule("r", RuleBackend::Any, ".*", ".*")];
        assert!(compute_desired_connections(&[], &rules).is_empty());
    }
}
