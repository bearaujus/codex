use pretty_assertions::assert_eq;

use super::*;

#[test]
fn fallback_classifies_generated_indexed_read_script() {
    let parsed = classify_powershell_script_fallback(
        "$p='src/app.rs'; $lines=Get-Content -LiteralPath $p; $lines[10..30]",
    )
    .expect("read-only script");

    assert_eq!(
        parsed,
        vec![ParsedCommand::Read {
            cmd: "Get-Content -LiteralPath src/app.rs".to_string(),
            name: "app.rs".to_string(),
            path: PathBuf::from("src/app.rs"),
        }]
    );
}

#[test]
fn fallback_classifies_generated_numbered_read_loops() {
    let parsed = classify_powershell_script_fallback(concat!(
        "$p='src/popup/App.tsx'; $lines=Get-Content -LiteralPath $p; ",
        "for($i=1230;$i -le 1305;$i++){ '{0,5}: {1}' -f ($i+1),$lines[$i] }; ",
        "$p='src/popup/browserBridge.ts'; $lines=Get-Content -LiteralPath $p; ",
        "for($i=1685;$i -le 1745;$i++){ '{0,5}: {1}' -f ($i+1),$lines[$i] }",
    ))
    .expect("generated numbered reads should be exploration");

    assert_eq!(
        parsed
            .iter()
            .filter_map(|item| match item {
                ParsedCommand::Read { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["App.tsx", "browserBridge.ts"]
    );
}

#[test]
fn fallback_classifies_generated_nested_range_read_loop() {
    let parsed = classify_powershell_script_fallback(concat!(
        "$p='src/popup/browserBridge.ts'; $lines=Get-Content -LiteralPath $p; ",
        "foreach($r in @(@(1625,1710),@(1745,1800))){",
        "for($i=$r[0];$i -le $r[1];$i++){ '{0,5}: {1}' -f ($i+1),$lines[$i] }}",
    ))
    .expect("generated multi-range read should be exploration");

    assert!(matches!(
        parsed.as_slice(),
        [ParsedCommand::Read { name, .. }] if name == "browserBridge.ts"
    ));
}

#[test]
fn fallback_classifies_bounded_generated_read_loops() {
    let parsed = classify_powershell_script_fallback(concat!(
        "$p='src/content.ts'; $lines=Get-Content -LiteralPath $p; ",
        "for($i=1000;$i -le 1125;$i++){ '{0,5}: {1}' -f ($i+1),$lines[$i] }; ",
        "for($i=1125;$i -le $lines.Length;$i++){ ",
        "if($i -gt 1185){break}; '{0,5}: {1}' -f ($i+1),$lines[$i] }",
    ))
    .expect("bounded generated reads should be exploration");

    assert!(matches!(
        parsed.as_slice(),
        [ParsedCommand::Read { name, .. }] if name == "content.ts"
    ));
}

#[test]
fn fallback_classifies_read_only_json_filter_and_projection() {
    let script = concat!(
        "$r=Get-Content -Raw -LiteralPath artifacts/diagnostic-report.json ",
        "| ConvertFrom-Json -Depth 100; ",
        "$auto=@($r.history.runs | Where-Object { ",
        "$_.mode -eq 'auto' -and $_.ok -eq $true } | Select-Object -First 3); ",
        "$auto | Format-Table startedAt,status",
    );
    let parsed = classify_powershell_script_fallback(script)
        .expect("read-only JSON projection should parse");

    assert!(matches!(
        parsed.as_slice(),
        [ParsedCommand::Read { name, .. }] if name == "diagnostic-report.json"
    ));

    let command = vec![
        "powershell.exe".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        script.to_string(),
    ];
    assert_eq!(
        presentation_parsed_commands(
            &command,
            vec![ParsedCommand::Unknown {
                cmd: script.to_string(),
            }],
        )
        .presentation,
        CommandPresentation::Inspection {
            target: "diagnostic-report.json".to_string(),
        }
    );
}

#[test]
fn presentation_classifier_recovers_wrapped_powershell_reads() {
    let command = vec![
        "powershell.exe".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        "$p='src/app.rs'; $lines=Get-Content -LiteralPath $p; $lines[10..30]".to_string(),
    ];
    let presented = presentation_parsed_commands(
        &command,
        vec![ParsedCommand::Unknown {
            cmd: command[3].clone(),
        }],
    );

    assert!(matches!(
        presented.parsed.as_slice(),
        [ParsedCommand::Read { name, .. }] if name == "app.rs"
    ));
    assert_eq!(presented.presentation, CommandPresentation::Exploration);
}

#[test]
fn fallback_classifies_read_pipeline_and_search() {
    let parsed = classify_powershell_script_fallback(
        "Get-Content -Raw -LiteralPath 'src/app.rs' | ConvertFrom-Json; rg -n TODO src",
    )
    .expect("read-only script");

    assert!(matches!(parsed[0], ParsedCommand::Read { .. }));
    assert!(matches!(parsed[1], ParsedCommand::Search { .. }));
}

#[test]
fn presentation_classifier_keeps_multiline_regex_searches_in_exploration() {
    let script = concat!(
        "rg -n -C 8 \"reducerTimestampCandidates|sentAt\\s*=|const sentAt|",
        "sendApprovedAction\\(|successfulEvents \\+=\" extension/src\n",
        "rg -n -C 16 'projectComposterAccelerated' ",
        "extension/tests/gameRules.integration.test.ts",
    );
    let command = vec![
        "powershell.exe".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        script.to_string(),
    ];
    let presented = presentation_parsed_commands(
        &command,
        vec![ParsedCommand::Unknown {
            cmd: script.to_string(),
        }],
    );

    assert_eq!(presented.presentation, CommandPresentation::Exploration);
    assert_eq!(
        presented
            .parsed
            .iter()
            .filter(|item| matches!(item, ParsedCommand::Search { .. }))
            .count(),
        2
    );
}

#[test]
fn fallback_rejects_mutating_or_mixed_script() {
    for script in [
        "Get-Content src/app.rs; Set-Content src/app.rs changed",
        "Get-Content src/app.rs > captured.txt",
        "Get-Content src/app.rs && Remove-Item src/app.rs",
        "Get-Content src/app.rs $(Remove-Item victim.txt)",
        "Get-Content src/app.rs | Sort-Object { Remove-Item victim.txt }",
        "$p=\"$(Remove-Item victim.txt)\"; Get-Content $p",
    ] {
        assert_eq!(
            classify_powershell_script_fallback(script),
            None,
            "mutating expression must stay visible as a normal command: {script}"
        );
    }
}

#[test]
fn fallback_rejects_mutation_hidden_in_generated_read_shapes() {
    for script in [
        concat!(
            "$p='src/app.rs';$lines=Get-Content $p;",
            "for($i=0;$i -le 2;$i++){$lines[$i]='changed';",
            "'{0}: {1}' -f $i,$lines[$i]}"
        ),
        concat!(
            "$p='src/app.rs';$lines=Get-Content $p;",
            "for($i=0;$i -le 2;$i++){Remove-Item victim.txt;",
            "'{0}: {1}' -f $i,$lines[$i]}"
        ),
        concat!(
            "$r=Get-Content capture.json -Raw|ConvertFrom-Json;",
            "$matches=$r.items|Where-Object { Remove-Item victim.txt }"
        ),
        concat!(
            "$r=Get-Content capture.json -Raw|ConvertFrom-Json;",
            "$r.items|ForEach-Object { Set-Content victim.txt $_ }"
        ),
    ] {
        assert_eq!(
            classify_powershell_script_fallback(script),
            None,
            "mixed mutation must remain a normal command: {script}"
        );
    }
}

#[test]
fn fallback_allows_quoted_search_metacharacters() {
    let parsed =
        classify_powershell_script_fallback("rg -n 'foo(bar)&baz>qux' src").expect("safe search");

    assert!(matches!(parsed.as_slice(), [ParsedCommand::Search { .. }]));
}

#[test]
fn fallback_rejects_mutation_through_a_read_result() {
    for script in [
        "$data=Get-Content capture.json -Raw|ConvertFrom-Json;$data.Save('out.json')",
        "$data=Get-Content capture.json -Raw|ConvertFrom-Json;$data.value = 1",
        "$data=Get-Content capture.json;$data|Set-Content out.txt",
    ] {
        assert_eq!(
            classify_powershell_script_fallback(script),
            None,
            "mutating expression must stay visible as a normal command: {script}"
        );
    }
}

#[test]
fn fallback_matches_powershell_variables_case_insensitively() {
    let parsed = classify_powershell_script_fallback(
        "$P='src/app.rs';$LINES=Get-Content -LiteralPath $p;$lines[0..2]",
    )
    .expect("PowerShell variables are case-insensitive");

    assert!(matches!(
        parsed.as_slice(),
        [ParsedCommand::Read { name, .. }] if name == "app.rs"
    ));
}

#[test]
fn read_only_git_is_presented_as_exploration() {
    assert!(matches!(
        classify_words(&["git".to_string(), "diff".to_string(), "--stat".to_string()]).as_deref(),
        Some([ParsedCommand::Search { .. }])
    ));
}

#[test]
fn structured_json_read_is_presented_as_inspection_even_when_upstream_is_mixed() {
    let command = vec![
        "powershell.exe".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        "$har = Get-Content -LiteralPath 'dev/composer boosted har' -Raw | ConvertFrom-Json -Depth 100; $har.actions".to_string(),
    ];
    let presented = presentation_parsed_commands(
        &command,
        vec![
            ParsedCommand::Read {
                cmd: "Get-Content".to_string(),
                name: "composer boosted har".to_string(),
                path: PathBuf::from("dev/composer boosted har"),
            },
            ParsedCommand::Unknown {
                cmd: "$har.actions".to_string(),
            },
        ],
    );

    assert_eq!(
        presented.presentation,
        CommandPresentation::Inspection {
            target: "composer boosted har".to_string(),
        }
    );
    assert!(matches!(
        presented.parsed.as_slice(),
        [ParsedCommand::Read { name, .. }] if name == "composer boosted har"
    ));
}
