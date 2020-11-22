//! Module dealing with actually running all the tests.

use crate::elm_json::{Config, Dependencies};
use glob::glob;
use regex::Regex;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{collections::HashSet, fs};
use std::{convert::TryFrom, path};

#[derive(Debug)]
/// Options passed as arguments.
pub struct Options {
    pub help: bool,
    pub version: bool,
    pub compiler: String,
    pub seed: u32,
    pub fuzz: u32,
    pub workers: u32,
    pub report: String,
    pub files: Vec<String>,
}

/// Main function, preparing and running the tests.
/// It has multiple steps that can be summarized as:
///
///  1. Generate the list of test modules and their file paths.
///  2. Generate a correct `elm.json` for the to-be-generated `Runner.elm`.
///  3. Compile all test files such that we know they are correct.
///  4. Find all tests.
///  5. Generate `Runner.elm` with a master test concatenating all found exposed tests.
///  6. Compile it into a JS file wrapped into a Node worker module.
///  7. Compile `Reporter.elm` into a Node module.
///  8. Generate and start the Node supervisor program.
pub fn main(options: Options) {
    // The help option is prioritary over the other options
    if options.help {
        crate::help::main();
        return;
    // The version option is the second priority
    } else if options.version {
        println!("{}", std::env!("CARGO_PKG_VERSION"));
        return;
    }

    // Verify that we are in an Elm project
    let elm_project_root = crate::utils::elm_project_root().unwrap();

    // Validate reporter
    let reporter = match options.report.as_ref() {
        "console" => "console".to_string(),
        "json" => "json".to_string(),
        "junit" => "junit".to_string(),
        value => {
            eprintln!("Wrong --report value: {}", value);
            crate::help::main();
            return;
        }
    };

    // Default with tests in the tests/ directory
    let module_globs = if options.files.is_empty() {
        let root_string = &elm_project_root.to_str().unwrap().to_string();
        vec![
            format!("{}/{}", root_string, "tests/*.elm"),
            format!("{}/{}", root_string, "tests/**/*.elm"),
        ]
    } else {
        options.files
    };

    // Get file paths of all modules in canonical form
    let module_paths: HashSet<PathBuf> = module_globs
        .iter()
        // join expanded globs for each pattern
        .flat_map(|pattern| {
            glob(pattern)
                .unwrap_or_else(|_| panic!(format!("Failed to read glob pattern {}", pattern)))
        })
        // filter out errors
        .filter_map(|x| x.ok())
        // canonical form of paths
        .map(|path| {
            path.canonicalize()
                .unwrap_or_else(|_| panic!(format!("Error in canonicalize of {:?}", path)))
        })
        // collect into a set of unique values
        .collect();

    // Read project elm.json
    let elm_json_str = std::fs::read_to_string(elm_project_root.join("elm.json"))
        .expect("Unable to read elm.json");
    let info = Config::try_from(elm_json_str.as_ref()).unwrap();

    // Convert package elm.json to an application elm.json if needed
    let mut elm_json_tests = match info {
        Config::Package(package) => crate::elm_json::ApplicationConfig::try_from(&package).unwrap(),
        Config::Application(application) => application,
    };

    // Make src dirs relative to the generated tests root
    let tests_root = elm_project_root.join("elm-stuff/tests-0.19.1");
    let elm_test_rs_root = crate::utils::elm_test_rs_root().unwrap();
    let test_directories: Vec<PathBuf> = elm_json_tests
        .source_directories
        .iter()
        // Add tests/ to the list of source directories
        .chain(std::iter::once(&"tests".to_string()))
        // Get canonical form
        .map(|path| elm_project_root.join(path).canonicalize().unwrap())
        // Get path relative to tests_root
        .collect();

    let source_directories_for_runner: Vec<PathBuf> = test_directories
        .iter()
        .map(|path| pathdiff::diff_paths(&path, &tests_root).expect("Could not get relative path"))
        // Add src/ and elm-test-rs/elm/src/ to the source directories
        .chain(vec![
            Path::new("src").into(),
            elm_test_rs_root.join("elm/src"),
        ])
        .collect();

    elm_json_tests.source_directories = source_directories_for_runner
        .iter()
        .map(|path| path.to_str().unwrap().to_string())
        .collect();

    // Promote test dependencies to normal ones
    elm_json_tests.promote_test_dependencies();

    // Write the elm.json file to disk
    let elm_json_tests_path = tests_root.join("elm.json");
    std::fs::create_dir_all(&tests_root.join("src")).expect("Could not create tests dir");
    std::fs::File::create(&elm_json_tests_path)
        .expect("Unable to create generated elm.json")
        .write_all(miniserde::json::to_string(&elm_json_tests).as_bytes())
        .expect("Unable to write to generated elm.json");

    // Finish preparing the elm.json file by solving any dependency issue (use elm-json)
    eprintln!("Running elm-json to solve dependency issues ...");
    let output = Command::new("elm-json")
        .arg("solve")
        .arg("--test")
        .arg("--extra")
        .arg("elm/core")
        .arg("elm/json")
        .arg("elm/time")
        .arg("elm/random")
        .arg("billstclair/elm-xml-eeue56")
        .arg("jorgengranseth/elm-string-format")
        .arg("--")
        .arg(&elm_json_tests_path)
        // stdio config
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .expect("command failed to start");
    let solved_dependencies: Dependencies =
        miniserde::json::from_str(std::str::from_utf8(&output.stdout).unwrap())
            .expect("Wrongly formed dependencies");
    elm_json_tests.dependencies = solved_dependencies;
    std::fs::File::create(&elm_json_tests_path)
        .expect("Unable to create generated elm.json")
        .write_all(miniserde::json::to_string(&elm_json_tests).as_bytes())
        .expect("Unable to write to generated elm.json");

    // Compile all test files
    eprintln!("Compiling all test files ...");
    compile(
        &tests_root,                        // current_dir
        &options.compiler,                  // compiler
        &Path::new("/dev/null").to_owned(), // output
        module_paths.iter(),                // src
    );

    // Find all modules and tests
    eprintln!("Finding all modules and tests ...");
    let all_modules_and_tests = crate::parser::all_tests(
        module_paths
            .iter()
            .map(|path| (path, fs::read_to_string(path).unwrap())),
    )
    .unwrap();

    let (runner_imports, maybe_runner_tests): (Vec<String>, Vec<String>) = all_modules_and_tests
        .iter()
        .map(|module| {
            let module_name = get_module_name(&test_directories, &module.path);
            let full_module_tests: Vec<String> = module
                .tests
                .iter()
                .map(|test| format!("check {}.{}", &module_name, test))
                .collect();
            let maybe_test = format!(
                r#"
      {{ module_ = "{}"
      , maybeTests =
            [ {}
            ]
      }}"#,
                &module_name,
                full_module_tests.join("\n            , ")
            )
            .trim()
            .to_string();
            ("import ".to_string() + &module_name, maybe_test)
        })
        .unzip();

    // Generate templated src/Runner.elm
    create_templated(
        elm_test_rs_root.join("templates/Runner.elm"), // template
        tests_root.join("src/Runner.elm"),             // output
        vec![
            ("user_imports".to_string(), runner_imports.join("\n")),
            ("tests".to_string(), maybe_runner_tests.join("\n    , ")),
        ],
    );

    // Compile the src/Runner.elm file into Runner.elm.js
    eprintln!("Compiling the generated templated src/Runner.elm ...");
    let compiled_elm_file = tests_root.join("js/Runner.elm.js");
    compile(
        &tests_root,         // current_dir
        &options.compiler,   // compiler
        &compiled_elm_file,  // output
        &["src/Runner.elm"], // src
    );

    fs::write(
        &compiled_elm_file,
        &add_kernel_test_checking(
            &fs::read_to_string(&compiled_elm_file).expect("Cannot read newly created elm.js file"),
        ),
    )
    .expect("Cannot write updated elm.js file");

    // Generate the node_runner.js node module embedding the Elm runner
    let polyfills = std::fs::read_to_string(&elm_test_rs_root.join("templates/node_polyfills.js"))
        .expect("polyfills.js template missing");
    let node_runner_path = tests_root.join("js/node_runner.js");
    create_templated(
        elm_test_rs_root.join("templates/node_runner.js"), // template
        node_runner_path.clone(),                          // output
        vec![
            ("polyfills".to_string(), polyfills.clone()),
            ("initialSeed".to_string(), options.seed.to_string()),
            ("fuzzRuns".to_string(), options.fuzz.to_string()),
        ],
    );

    // Compile the Reporter.elm into Reporter.elm.js
    eprintln!("Compiling Reporter.elm.js ...");
    let compiled_reporter = tests_root.join("js/Reporter.elm.js");
    compile(
        &tests_root,        // current_dir
        &options.compiler,  // compiler
        &compiled_reporter, // output
        &[elm_test_rs_root.join("templates/Reporter.elm")],
    );

    // Generate the supervisor Node module
    create_templated(
        elm_test_rs_root.join("templates/node_supervisor.js"), // template
        tests_root.join("js/node_supervisor.js"),              // output
        vec![
            ("polyfills".to_string(), polyfills),
            ("nb_workers".to_string(), options.workers.to_string()),
            ("initialSeed".to_string(), options.seed.to_string()),
            ("fuzzRuns".to_string(), options.fuzz.to_string()),
            ("reporter".to_string(), reporter),
        ],
    );

    // Start the tests supervisor
    eprintln!("Starting the supervisor ...");
    let mut supervisor = Command::new("node")
        .arg("js/node_supervisor.js")
        .current_dir(&tests_root)
        .stdin(Stdio::piped())
        .spawn()
        .expect("command failed to start");

    // Helper closure to write to supervisor
    let stdin = supervisor.stdin.as_mut().expect("Failed to open stdin");
    let mut writeln = |msg| {
        stdin.write_all(msg).expect("writeln");
        stdin.write_all(b"\n").expect("writeln");
    };

    // Send runner module path to supervisor to start the work
    eprintln!("Running tests ...");
    let node_runner_path_string = node_runner_path.to_str().unwrap().to_string();
    writeln(&node_runner_path_string.as_bytes());

    // Wait for supervisor child process to end and terminate with same exit code
    let exit_code = wait_child(&mut supervisor);
    eprintln!("Exited with code {:?}", exit_code);
    std::process::exit(exit_code.unwrap_or(1));
}

/// Wait for child process to end
fn wait_child(child: &mut std::process::Child) -> Option<i32> {
    match child.try_wait() {
        Ok(Some(status)) => status.code(),
        Ok(None) => match child.wait() {
            Ok(status) => status.code(),
            _ => None,
        },
        Err(e) => {
            eprintln!("Error attempting to wait for child: {}", e);
            None
        }
    }
}

/// Compile an Elm module into a JS file (without --optimized)
fn compile<P, I, S>(current_dir: P, compiler: &str, output: P, src: I)
where
    P: AsRef<Path>,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let status = Command::new(compiler)
        .arg("make")
        .arg(format!("--output={}", output.as_ref().to_str().unwrap()))
        .args(src)
        .current_dir(current_dir)
        // stdio config, comment to see elm make output for debug
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .expect("Command elm make failed to start");
    if !status.success() {
        std::process::exit(1);
    }
}

/// Replace the template keys and write result to output file.
fn create_templated<P: AsRef<Path>>(template: P, output: P, replacements: Vec<(String, String)>) {
    let template_content = std::fs::read_to_string(template).expect("template missing");
    let content = varj::parse(&template_content, &replacements.into_iter().collect())
        .expect("The template does not match with the replacement keys");
    std::fs::File::create(output)
        .expect("Unable to create generated file")
        .write_all(content.as_bytes())
        .expect("Unable to write to generated file");
}

fn add_kernel_test_checking(elm_js: &str) -> String {
    lazy_static::lazy_static! {

        /// For older versions of elm-explorations/test we need to list every single
        /// variant of the `Test` type. To avoid having to update this regex if a new
        /// variant is added, newer versions of elm-explorations/test have prefixed all
        /// variants with `ElmTestVariant__` so we can match just on that.
        /// TODO(harry): ask Lydell if the \s*\$:\s*(['"])\1\2 bit is important.
        /// I had to remove this from the end because the regex crate does not
        /// support them.
        static ref TEST_VARIANT_DEFINITION: Regex = Regex::new(r#"(?m)^var\s+\$elm_explorations\$test\$Test\$Internal\$(?:ElmTestVariant__\w+|UnitTest|FuzzTest|Labeled|Skipped|Only|Batch)\s*=\s*(?:\w+\(\s*)?function\s*\([\w, ]*\)\s*\{\s*return\s*\{"#).unwrap();

        static ref CHECK_DEFINITION: Regex = Regex::new(r#"(?m)^(var\s+\$author\$project\$Runner\$check)\s*=\s*\$author\$project\$Runner\$checkHelperReplaceMe___;?$"#).unwrap();
    }

    let elm_js =
        TEST_VARIANT_DEFINITION.replace_all(&elm_js, "$0 __elmTestSymbol: __elmTestSymbol,");
    let elm_js = CHECK_DEFINITION.replace(&elm_js, "$1 = value => value && value.__elmTestSymbol === __elmTestSymbol ? $$elm$$core$$Maybe$$Just(value) : $$elm$$core$$Maybe$$Nothing;");

    ["const __elmTestSymbol = Symbol('elmTestSymbol');", &elm_js].join("\n")
}

fn get_module_name(
    source_dirs: impl IntoIterator<Item = impl AsRef<Path>> + Clone,
    test_file: impl AsRef<Path>,
) -> String {
    let matching_source_dir = {
        let mut matching_dir_iter = source_dirs
            .into_iter()
            .filter(|dir| test_file.as_ref().starts_with(&dir));
        if let Some(dir) = matching_dir_iter.next() {
            let extra: Vec<_> = matching_dir_iter.collect();
            if !extra.is_empty() {
                panic!("2+ matching source dirs")
            }
            dir
        } else {
            panic!(
                "This file:
{}
...matches no source directory! Imports won’t work then.
",
                test_file.as_ref().display()
            )
        }
    };

    // By finding the module name from the file path we can import it even if
    // the file is full of errors. Elm will then report what’s wrong.
    let module_name_parts = test_file
        .as_ref()
        .strip_prefix(matching_source_dir)
        .unwrap()
        .components()
        .map(|c| match c {
            path::Component::Normal(s) => s.to_str().unwrap().replace(".elm", ""),
            _ => panic!(),
        })
        .collect::<Vec<_>>();

    assert!(module_name_parts.iter().all(|s| is_upper_name(s)));
    assert!(!module_name_parts.is_empty());
    module_name_parts.join(".")
}

fn is_upper_name(s: &str) -> bool {
    lazy_static::lazy_static! {
        static ref UPPER_NAME: Regex = Regex::new(r"^\p{Lu}[_\d\p{L}]*$").unwrap();
    }
    UPPER_NAME.is_match(s)
}
