//! **The battery leaves the operator's harness config as it found it.** A row whose TUI keeps a
//! boot dialog's answer in the operator's own config (`Remembers::OperatorConfig`) has P-tui run
//! on a config moved into the probe's world. Proved here against a canned stand-in, not the real
//! harness: the stand-in shows the row's own dialog and, answered, writes the answer where the row
//! says the real one does — `$<config>/<file>`, or `$HOME/<file>` with the variable unset. HOME is
//! a scratch directory for the stand-in alone; no real harness, and no login, is involved.

use std::path::Path;

use marion_harness::spec::{DialogAnswer, Relocation, Remembers};

use crate::{probes, target};

/// Mutation: drop the relocation in `target::compile`. The stand-in then writes its answer into the
/// fake HOME's file and the byte comparison fails.
#[test]
fn a_p_tui_run_leaves_the_operators_config_byte_identical() {
    let scratch = marion_testsupport::scratch("conformance-hygiene");
    let mut checked = Vec::new();
    for (selector, built) in target::all() {
        let Ok(t) = built else { continue };
        let Remembers::OperatorConfig(r) = t.spec.boot_dialogs.remembers else {
            continue;
        };
        let dialog = t
            .spec
            .boot_dialogs
            .dialogs
            .iter()
            .find(|d| matches!(d.answer, DialogAnswer::Keys(_)))
            .unwrap_or_else(|| panic!("{selector}: keeps an answer to a dialog it never answers"));
        let row = scratch.join(selector.replace(':', "-"));
        let home = row.join("home");
        std::fs::create_dir_all(&home).expect("fake HOME");
        let (file, _) = r
            .seed
            .first()
            .unwrap_or_else(|| panic!("{selector}: a relocation seeds the file it moves"));
        let operator = home.join(file);
        std::fs::write(&operator, "{\"projects\":{}}\n").expect("the operator's config");
        let before = std::fs::read(&operator).expect("read it back");
        let wrote = row.join("stand-in-wrote");
        let mut c = probes::Ctx {
            t: &t,
            out: row.join("out"),
            scratch: row.join("worlds"),
            report_answered: None,
            stand_in: Some(probes::StandIn {
                program: stand_in(&row, dialog.needle, &r),
                env: vec![
                    ("HOME".into(), home.display().to_string()),
                    ("MARION_STAND_IN_WROTE".into(), wrote.display().to_string()),
                ],
            }),
        };
        let o = probes::p_tui(&mut c);
        // The stand-in took P-tui's whole path, dialog answered, so the comparison below is about
        // a run that wrote an answer somewhere.
        assert_eq!(
            o.status,
            crate::report::Status::Pass,
            "{selector}: {}",
            o.observed
        );
        let written = std::fs::read_to_string(&wrote).unwrap_or_else(|e| {
            panic!(
                "{selector}: the stand-in's dialog was never answered ({e}); P-tui: {}",
                o.observed
            )
        });
        let written = Path::new(written.trim());
        assert!(
            written != operator && written.starts_with(row.join("worlds")),
            "{selector}: the answer went to {}, not into the probe's world",
            written.display()
        );
        assert_eq!(
            std::fs::read(&operator).expect("the operator's config is still there"),
            before,
            "{selector}: P-tui changed the operator's {file}"
        );
        checked.push(selector);
    }
    assert!(
        !checked.is_empty(),
        "no row states Remembers::OperatorConfig, so nothing here was proved"
    );
}

/// A TUI reduced to P-tui's path: bracketed paste on, the row's dialog on the first screen, the
/// first line typed is its answer (kept as described above), a later line naming a marker is a
/// turn sent to the probe's provider, and `/mcp` lists marion.
fn stand_in(dir: &Path, needle: &str, r: &Relocation) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    assert!(!needle.contains('\''), "the needle is quoted in sh");
    let (file, _) = r.seed[0];
    let script = format!(
        r#"#!/bin/sh
dir=${{{config}:-$HOME}}
printf '\033[?2004h%s\r\n' '{needle}'
answered=
while IFS= read -r line; do
    if [ -z "$answered" ]; then
        answered=1
        printf '{{"projects":{{"%s":{{"trusted":true}}}}}}\n' "$PWD" > "$dir/{file}"
        printf '%s\n' "$dir/{file}" > "$MARION_STAND_IN_WROTE"
        printf '\033[2J\033[H> \r\n'
        continue
    fi
    case $line in
        */mcp*) printf 'marion\r\n' ;;
        *)
            text=$(printf %s "$line" | tr -cd 'A-Za-z0-9 :.')
            curl -s -m 10 -o /dev/null -H 'content-type: application/json' --data-binary \
                "{{\"max_tokens\":8,\"tools\":[{{\"name\":\"t\",\"input_schema\":{{}}}}],\"messages\":[{{\"role\":\"user\",\"content\":\"$text\"}}]}}" \
                "$MARION_STAND_IN_PROVIDER/v1/messages"
            printf 'done\r\n'
            ;;
    esac
done
"#,
        config = r.config,
    );
    let path = dir.join("stand-in");
    std::fs::write(&path, script).expect("write the stand-in");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}
