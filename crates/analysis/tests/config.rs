//! Config file parsing, writing, and round trips.

use analysis::{Config, ConfigError, FunctionSpec, State};

#[test]
fn parse_handles_comments_names_and_modules() {
    let text = "# known functions\n\
                \n\
                arm_func 0x08000000 entry\n\
                thumb_func 0x080000C0\n\
                main.s arm 0x08000024 arm_helper\n\
                main.s thumb 0x080000E0 thumb_helper\n";
    let config = Config::parse(text).expect("parse");
    assert_eq!(
        config,
        Config {
            functions: vec![
                FunctionSpec {
                    addr: 0x0800_0000,
                    state: State::Arm,
                    name: Some("entry".into()),
                    module: None,
                },
                FunctionSpec {
                    addr: 0x0800_00C0,
                    state: State::Thumb,
                    name: None,
                    module: None,
                },
                FunctionSpec {
                    addr: 0x0800_0024,
                    state: State::Arm,
                    name: Some("arm_helper".into()),
                    module: Some("main".into()),
                },
                FunctionSpec {
                    addr: 0x0800_00E0,
                    state: State::Thumb,
                    name: Some("thumb_helper".into()),
                    module: Some("main".into()),
                },
            ],
            tables: Vec::new(),
        }
    );
}

#[test]
fn table_hints_parse_and_round_trip() {
    let text = "# switch hints\narm_table 0x08000100 8\nthumb_table 0x08000200 4\n";
    let config = Config::parse(text).expect("parse");
    assert_eq!(
        config.tables,
        vec![
            analysis::TableHint {
                addr: 0x0800_0100,
                entries: 8,
                state: analysis::State::Arm,
            },
            analysis::TableHint {
                addr: 0x0800_0200,
                entries: 4,
                state: analysis::State::Thumb,
            },
        ]
    );
    assert_eq!(
        config.to_string(),
        "arm_table 0x08000100 8\nthumb_table 0x08000200 4\n"
    );
    assert_eq!(Config::parse(&config.to_string()).expect("reparse"), config);
    // Bad entry counts and wrong arity are typed errors.
    match Config::parse("arm_table 0x08000100 xx\n") {
        Err(ConfigError::Syntax { line: 1 }) => {}
        other => panic!("expected syntax error, got {other:?}"),
    }
    match Config::parse("arm_table 0x08000100\n") {
        Err(ConfigError::Syntax { line: 1 }) => {}
        other => panic!("expected syntax error, got {other:?}"),
    }
    match Config::parse("thumb_table 0xZZ 4\n") {
        Err(ConfigError::Address { line: 1, .. }) => {}
        other => panic!("expected address error, got {other:?}"),
    }
}

#[test]
fn module_line_without_state_token_defaults_to_arm() {
    let config = Config::parse("main.s 0x08000024\n").expect("parse");
    assert_eq!(config.functions.len(), 1);
    assert_eq!(config.functions[0].state, State::Arm);
    assert_eq!(config.functions[0].module.as_deref(), Some("main"));
}

#[test]
fn invalid_lines_produce_typed_errors() {
    for case in ["bogus 0x08000000", "arm_func 0x08000000 a b", "arm_func"] {
        match Config::parse(&format!("{case}\n")) {
            Err(ConfigError::Syntax { line: 1 }) => {}
            other => panic!("expected syntax error for {case:?}, got {other:?}"),
        }
    }
    for case in ["arm_func 08000000", "arm_func 0xZZZZ"] {
        match Config::parse(&format!("{case}\n")) {
            Err(ConfigError::Address { line: 1, .. }) => {}
            other => panic!("expected address error for {case:?}, got {other:?}"),
        }
    }
    match Config::parse(".s 0x08000000\n") {
        Err(ConfigError::Name { line: 1, .. }) => {}
        other => panic!("expected name error, got {other:?}"),
    }
    // A function name may be any whitespace-free token; assembler
    // identifier rules are applied later, at emission, with a warning.
    let loose = Config::parse("arm_func 0x08000000 9bad-name\n").expect("loose name parses");
    assert_eq!(loose.functions[0].name.as_deref(), Some("9bad-name"));
    // Line numbers survive leading blank and comment lines.
    match Config::parse("# comment\n\nnope\n") {
        Err(ConfigError::Syntax { line: 3 }) => {}
        other => panic!("expected syntax error on line 3, got {other:?}"),
    }
}

#[test]
fn display_writes_canonical_text_and_round_trips() {
    let config = Config::parse(
        "arm_func 0x08000000 entry\n\
         thumb_func 0x080000C0\n\
         main.s 0x08000024 arm_helper\n\
         main.s thumb 0x080000E0 thumb_helper\n",
    )
    .expect("parse");
    let text = config.to_string();
    assert_eq!(
        text,
        "arm_func 0x08000000 entry\n\
         thumb_func 0x080000c0\n\
         main.s arm 0x08000024 arm_helper\n\
         main.s thumb 0x080000e0 thumb_helper\n"
    );
    assert_eq!(Config::parse(&text).expect("reparse"), config);
}

#[test]
fn load_and_save_round_trip_through_a_file() {
    let dir = std::env::temp_dir().join("oxyboy-config-test");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("functions.cfg");
    let config = Config::parse("thumb_func 0x080000C0 thumb_helper\n").expect("parse");
    config.save(&path).expect("save");
    let loaded = Config::load(&path).expect("load");
    assert_eq!(loaded, config);
    std::fs::remove_file(&path).expect("cleanup");
}

#[test]
fn sanitize_identifier_rewrites_illegal_characters() {
    use analysis::config::sanitize_identifier;
    assert_eq!(sanitize_identifier("my-func"), "my_func");
    assert_eq!(sanitize_identifier("9bad"), "_9bad");
    assert_eq!(sanitize_identifier("we ird"), "we_ird");
    assert_eq!(sanitize_identifier("dot.name"), "dot_name");
    assert_eq!(sanitize_identifier("_ok2"), "_ok2");
    assert_eq!(sanitize_identifier(""), "_");
}

#[test]
fn names_and_modules_survive_the_config_round_trip() {
    let config =
        Config::parse("split.s thumb 0x020000e4 my-func\narm_func 0x08000000 x\n").expect("parse");
    let text = config.to_string();
    let reloaded = Config::parse(&text).expect("reparse");
    assert_eq!(reloaded, config);
    assert_eq!(reloaded.functions[0].name.as_deref(), Some("my-func"));
    assert_eq!(reloaded.functions[0].module.as_deref(), Some("split"));
}
