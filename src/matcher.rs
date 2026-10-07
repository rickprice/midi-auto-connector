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
/// Likewise, a rule's `kind` restricts it to ports of that kind only --
/// `output`/`input` never match a port of the other kind, so a rule can
/// never cross-wire an audio port to a MIDI one.
///
/// If a rule's `output` and `input` regexes each contain exactly one
/// capture group, a source/dest pair is only connected when the two
/// captured substrings are equal -- e.g. `output = "out-(\d+)"` paired
/// with `input = "in-(\d+)"` connects `out-1` to `in-1` and `out-2` to
/// `in-2`, but never `out-1` to `in-2`. The comparison is a plain string
/// equality on whatever text each group captures, so it works just as
/// well for labels (`_FL`/`_FR`) as for numeric indices. With zero or
/// more than one capture group on either side, matching falls back to a
/// full cross-join of every `output` match against every `input` match.
///
/// Pure and deterministic: the same `ports` and `rules` always produce the
/// same result, in rule order, then source order, then dest order (as
/// given in `ports`).
pub fn compute_desired_connections(ports: &[PortInfo], rules: &[Rule]) -> Vec<DesiredConnection> {
    let mut out = Vec::new();
    for rule in rules {
        let pair_by_capture = rule.output.captures_len() == 2 && rule.input.captures_len() == 2;

        for source in ports
            .iter()
            .filter(|p| rule.backend.applies_to(p.backend()))
            .filter(|p| rule.kind.matches(p.kind))
            .filter(|p| p.direction.can_be_source)
        {
            let source_name = source.full_name();
            let Some(source_caps) = rule.output.captures(&source_name) else {
                continue;
            };
            let source_key = pair_by_capture.then(|| source_caps[1].to_string());

            for dest in ports
                .iter()
                .filter(|p| p.backend() == source.backend())
                .filter(|p| rule.kind.matches(p.kind))
                .filter(|p| p.direction.can_be_sink)
                .filter(|p| p.id != source.id)
            {
                let dest_name = dest.full_name();
                let Some(dest_caps) = rule.input.captures(&dest_name) else {
                    continue;
                };
                if let Some(source_key) = &source_key
                    && &dest_caps[1] != source_key
                {
                    continue;
                }

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
    use crate::config::{RuleBackend, RuleKind};
    use crate::port::{PortDirection, PortKind};
    use regex::Regex;

    fn rule(name: &str, backend: RuleBackend, output: &str, input: &str) -> Rule {
        rule_kind(name, backend, RuleKind::Midi, output, input)
    }

    fn rule_kind(
        name: &str,
        backend: RuleBackend,
        kind: RuleKind,
        output: &str,
        input: &str,
    ) -> Rule {
        Rule {
            name: name.to_string(),
            backend,
            kind,
            output: Regex::new(output).unwrap(),
            input: Regex::new(input).unwrap(),
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
            kind: PortKind::Midi,
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
        pw_port_kind(
            node_id,
            port_id,
            client_name,
            port_name,
            src,
            sink,
            PortKind::Midi,
        )
    }

    fn pw_port_kind(
        node_id: u32,
        port_id: u32,
        client_name: &str,
        port_name: &str,
        src: bool,
        sink: bool,
        kind: PortKind,
    ) -> PortInfo {
        PortInfo {
            id: PortId::PipeWire { node_id, port_id },
            client_name: client_name.to_string(),
            port_name: port_name.to_string(),
            direction: PortDirection {
                can_be_source: src,
                can_be_sink: sink,
            },
            kind,
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

    #[test]
    fn single_capture_group_pairs_by_equal_captured_text() {
        let ports = vec![
            alsa_port(1, 0, "Mixer", "out-1", true, false),
            alsa_port(1, 1, "Mixer", "out-2", true, false),
            alsa_port(2, 0, "Mixer", "in-1", false, true),
            alsa_port(2, 1, "Mixer", "in-2", false, true),
        ];
        let rules = vec![rule(
            "paired",
            RuleBackend::Alsa,
            r"^Mixer:out-(\d+)$",
            r"^Mixer:in-(\d+)$",
        )];

        let got = compute_desired_connections(&ports, &rules);
        assert_eq!(got.len(), 2);
        assert!(
            got.iter()
                .any(|c| c.source == ports[0].id && c.dest == ports[2].id)
        );
        assert!(
            got.iter()
                .any(|c| c.source == ports[1].id && c.dest == ports[3].id)
        );
    }

    #[test]
    fn single_capture_group_pairs_by_label_not_just_digits() {
        let ports = vec![
            alsa_port(1, 0, "Mixer", "out-_FL", true, false),
            alsa_port(1, 1, "Mixer", "out-_FR", true, false),
            alsa_port(2, 0, "Mixer", "in-_FL", false, true),
            alsa_port(2, 1, "Mixer", "in-_FR", false, true),
        ];
        let rules = vec![rule(
            "paired-labels",
            RuleBackend::Alsa,
            r"^Mixer:out-(_F[LR])$",
            r"^Mixer:in-(_F[LR])$",
        )];

        let got = compute_desired_connections(&ports, &rules);
        assert_eq!(got.len(), 2);
        assert!(
            got.iter()
                .any(|c| c.source == ports[0].id && c.dest == ports[2].id)
        );
        assert!(
            got.iter()
                .any(|c| c.source == ports[1].id && c.dest == ports[3].id)
        );
    }

    #[test]
    fn zero_or_multiple_capture_groups_still_fall_back_to_cross_join() {
        let ports = vec![
            alsa_port(1, 0, "Mixer", "out-1", true, false),
            alsa_port(2, 0, "Mixer", "in-1", false, true),
            alsa_port(2, 1, "Mixer", "in-2", false, true),
        ];
        // No capture group on the output side -> no pairing, full fan-out.
        let rules = vec![rule(
            "no-groups",
            RuleBackend::Alsa,
            r"^Mixer:out-\d+$",
            r"^Mixer:in-(\d+)$",
        )];

        let got = compute_desired_connections(&ports, &rules);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn midi_rule_ignores_audio_ports_even_with_matching_names() {
        let ports = vec![
            pw_port_kind(1, 0, "Device", "out", true, false, PortKind::Audio),
            pw_port_kind(2, 0, "Device", "in", false, true, PortKind::Audio),
        ];
        let rules = vec![rule(
            "midi-only",
            RuleBackend::PipeWire,
            "^Device",
            "^Device",
        )];

        let got = compute_desired_connections(&ports, &rules);
        assert!(
            got.is_empty(),
            "a midi rule must not match audio ports, even with identical names"
        );
    }

    #[test]
    fn audio_rule_ignores_midi_ports_even_with_matching_names() {
        let ports = vec![
            pw_port(1, 0, "Device", "out", true, false),
            pw_port(2, 0, "Device", "in", false, true),
        ];
        let rules = vec![rule_kind(
            "audio-only",
            RuleBackend::PipeWire,
            RuleKind::Audio,
            "^Device",
            "^Device",
        )];

        let got = compute_desired_connections(&ports, &rules);
        assert!(
            got.is_empty(),
            "an audio rule must not match midi ports, even with identical names"
        );
    }

    #[test]
    fn audio_rule_connects_audio_ports_with_channel_pairing() {
        let ports = vec![
            pw_port_kind(1, 0, "Interface", "output_FL", true, false, PortKind::Audio),
            pw_port_kind(1, 1, "Interface", "output_FR", true, false, PortKind::Audio),
            pw_port_kind(
                2,
                0,
                "Speakers",
                "playback_FL",
                false,
                true,
                PortKind::Audio,
            ),
            pw_port_kind(
                2,
                1,
                "Speakers",
                "playback_FR",
                false,
                true,
                PortKind::Audio,
            ),
        ];
        let rules = vec![rule_kind(
            "audio-channels",
            RuleBackend::PipeWire,
            RuleKind::Audio,
            r"^Interface:output_(FL|FR)$",
            r"^Speakers:playback_(FL|FR)$",
        )];

        let got = compute_desired_connections(&ports, &rules);
        assert_eq!(got.len(), 2);
        assert!(
            got.iter()
                .any(|c| c.source == ports[0].id && c.dest == ports[2].id)
        );
        assert!(
            got.iter()
                .any(|c| c.source == ports[1].id && c.dest == ports[3].id)
        );
    }
}
