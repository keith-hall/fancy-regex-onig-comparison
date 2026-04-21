use std::collections::HashMap;
use std::fs;

/// A `match` pattern extracted from the HAML sublime-syntax file.
#[derive(Debug)]
struct MatchPattern {
    /// The context the pattern was found in (for reporting).
    context: String,
    /// The expanded regex pattern (variables already substituted).
    pattern: String,
    /// Whether the rule has an action (push/pop/set/embed/branch/fail).
    /// Rules without an action should use the `find_not_empty` flag.
    has_action: bool,
}

/// Capture group results for a single match attempt:
/// `None` means no match; `Some(groups)` means a match where `groups[0]` is
/// the overall match span and `groups[i]` for `i > 0` is capture group `i`
/// (`None` if that group did not participate in the match).
type Captures = Option<Vec<Option<(usize, usize)>>>;

/// A single difference between the two engines for one (pattern, line) pair.
#[derive(Debug)]
struct Difference {
    pattern: String,
    context: String,
    has_action: bool,
    line_num: usize,
    line: String,
    fancy_captures: Captures,
    onig_captures: Captures,
}

/// Extract the `variables` mapping from the parsed YAML document.
fn extract_variables(doc: &serde_yaml::Value) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    if let Some(mapping) = doc.get("variables").and_then(|v| v.as_mapping()) {
        for (key, value) in mapping {
            if let (Some(k), Some(v)) = (key.as_str(), value.as_str()) {
                vars.insert(k.to_string(), v.to_string());
            }
        }
    }
    vars
}

/// Expand `{{variable}}` references in a pattern string.
fn expand_variables(pattern: &str, variables: &HashMap<String, String>) -> String {
    let mut result = pattern.to_string();
    // Repeatedly expand until no more substitutions can be made (handles nested vars).
    loop {
        let mut changed = false;
        for (name, value) in variables {
            let placeholder = format!("{{{{{}}}}}", name);
            if result.contains(&placeholder) {
                result = result.replace(&placeholder, value);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    result
}

/// Parse all `match` rules from every context in the YAML document.
fn extract_match_patterns(
    doc: &serde_yaml::Value,
    variables: &HashMap<String, String>,
) -> Vec<MatchPattern> {
    let mut patterns = Vec::new();

    let contexts = match doc.get("contexts").and_then(|v| v.as_mapping()) {
        Some(m) => m,
        None => return patterns,
    };

    for (ctx_name, ctx_rules) in contexts {
        let ctx_name = ctx_name.as_str().unwrap_or("<unknown>");
        let rules = match ctx_rules.as_sequence() {
            Some(r) => r,
            None => continue,
        };

        for rule in rules {
            // Only consider rules with a `match` key.
            let raw_pattern = match rule.get("match").and_then(|v| v.as_str()) {
                Some(p) => p,
                None => continue,
            };

            let pattern = expand_variables(raw_pattern, variables);

            // A rule "has an action" if it contains any of these keys.
            // Rules without an action are the ones that just apply scope/captures
            // and continue in the same context.
            let has_action = rule.get("push").is_some()
                || rule.get("pop").is_some()
                || rule.get("set").is_some()
                || rule.get("embed").is_some()
                || rule.get("branch").is_some()
                || rule.get("fail").is_some();

            patterns.push(MatchPattern {
                context: ctx_name.to_string(),
                pattern,
                has_action,
            });
        }
    }

    patterns
}

/// Compile an onig regex with the appropriate options.
/// `REGEX_OPTION_CAPTURE_GROUP` is always included so that unnamed capture
/// groups are tracked even when named groups are also present in the pattern.
fn compile_onig(pattern: &str, find_not_empty: bool) -> Result<onig::Regex, onig::Error> {
    let mut options = onig::RegexOptions::REGEX_OPTION_CAPTURE_GROUP;
    if find_not_empty {
        options |= onig::RegexOptions::REGEX_OPTION_FIND_NOT_EMPTY;
    }
    onig::Regex::with_options(pattern, options, onig::Syntax::default())
}

/// Run onig against `line` and return all capture group spans.
/// Returns `None` when there is no match, or `Some(groups)` where `groups[0]`
/// is the overall match and `groups[i]` for `i > 0` is capture group `i`.
fn onig_search(regex: &onig::Regex, line: &str) -> Captures {
    let mut region = onig::Region::new();
    let found = regex.search_with_options(
        line,
        0,
        line.len(),
        onig::SearchOptions::SEARCH_OPTION_NONE,
        Some(&mut region),
    );
    found.map(|_| (0..region.len()).map(|i| region.pos(i)).collect())
}

#[test]
fn compare_haml_patterns_vs_onig() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let syntax_path =
        manifest_dir.join("testdata/Packages/Rails/HAML.sublime-syntax");
    let test_path =
        manifest_dir.join("testdata/Packages/Rails/tests/syntax_test_rails.haml");

    let syntax_content = fs::read_to_string(&syntax_path)
        .expect("could not read HAML.sublime-syntax");
    let doc: serde_yaml::Value =
        serde_yaml::from_str(&syntax_content).expect("could not parse HAML.sublime-syntax");

    let variables = extract_variables(&doc);
    let patterns = extract_match_patterns(&doc, &variables);

    let haystack = fs::read_to_string(&test_path)
        .expect("could not read syntax_test_rails.haml");
    // Collect lines; we keep the owned strings so we can pass &str to both engines.
    let lines: Vec<&str> = haystack.lines().collect();

    let mut differences: Vec<Difference> = Vec::new();
    let mut compile_errors: Vec<String> = Vec::new();
    let mut patterns_tested: usize = 0;

    for mp in &patterns {
        let find_not_empty = !mp.has_action;

        // --- Compile with fancy-regex ---
        let fancy_compile = fancy_regex::RegexBuilder::new(&mp.pattern)
            .oniguruma_mode(true)
            .find_not_empty(find_not_empty)
            .build();

        // PatternCanNeverMatch means the combination of (pattern, find_not_empty=true)
        // can never return a result.  We treat this as "no match for every line" so
        // that we can still compare against onig's runtime behaviour.
        let fancy_regex: Option<fancy_regex::Regex> = match fancy_compile {
            Ok(r) => Some(r),
            Err(fancy_regex::Error::CompileError(e))
                if matches!(*e, fancy_regex::CompileError::PatternCanNeverMatch) =>
            {
                None
            }
            Err(e) => {
                // Even when fancy-regex fails to compile, try onig.  If onig can compile
                // and find matches, every matching line counts as a difference (fancy-regex
                // produces no result where onig does).
                let onig_result = compile_onig(&mp.pattern, find_not_empty);
                let onig_status = match &onig_result {
                    Ok(_) => "onig compiled OK".to_string(),
                    Err(oe) => format!("onig also failed: {}", oe),
                };
                compile_errors.push(format!(
                    "fancy-regex compile error for {:?} (context={}, find_not_empty={}): {} [{}]",
                    mp.pattern, mp.context, find_not_empty, e, onig_status
                ));
                if let Ok(onig_re) = onig_result {
                    patterns_tested += 1;
                    for (line_idx, line) in lines.iter().enumerate() {
                        let onig_captures = onig_search(&onig_re, line);
                        if onig_captures.is_some() {
                            differences.push(Difference {
                                pattern: mp.pattern.clone(),
                                context: mp.context.clone(),
                                has_action: mp.has_action,
                                line_num: line_idx + 1,
                                line: line.to_string(),
                                fancy_captures: None,
                                onig_captures,
                            });
                        }
                    }
                }
                continue;
            }
        };

        // --- Compile with onig ---
        // In Oniguruma, FIND_NOT_EMPTY is a compile-time regex option (RegexOptions),
        // so we compile the pattern with the flag set when appropriate.
        let onig_regex = match compile_onig(&mp.pattern, find_not_empty) {
            Ok(r) => r,
            Err(e) => {
                compile_errors.push(format!(
                    "onig compile error for {:?} (context={}, find_not_empty={}): {}",
                    mp.pattern, mp.context, find_not_empty, e
                ));
                continue;
            }
        };

        patterns_tested += 1;

        // --- Compare results for every line ---
        for (line_idx, line) in lines.iter().enumerate() {
            // fancy-regex: capture all groups from position 0
            let fancy_captures: Captures = match &fancy_regex {
                None => None,
                Some(re) => re.captures(line).ok().flatten().map(|caps| {
                    (0..caps.len())
                        .map(|i| caps.get(i).map(|m| (m.start(), m.end())))
                        .collect()
                }),
            };

            let onig_captures = onig_search(&onig_regex, line);

            if fancy_captures != onig_captures {
                differences.push(Difference {
                    pattern: mp.pattern.clone(),
                    context: mp.context.clone(),
                    has_action: mp.has_action,
                    line_num: line_idx + 1,
                    line: line.to_string(),
                    fancy_captures,
                    onig_captures,
                });
            }
        }
    }

    eprintln!(
        "\nTested {} patterns × {} lines.",
        patterns_tested,
        lines.len()
    );

    // Always print compile errors so they are visible in the test output.
    if !compile_errors.is_empty() {
        eprintln!("\n=== Compile errors ({}) ===", compile_errors.len());
        for e in &compile_errors {
            eprintln!("  {}", e);
        }
    }

    // Print every difference, then fail if any were found.
    if !differences.is_empty() {
        eprintln!("\n=== Differences ({}) ===", differences.len());
        for d in &differences {
            eprintln!(
                "Pattern:     {:?}\n\
                 Context:     {} (has_action={}, find_not_empty={})\n\
                 Line {:>4}:  {:?}\n\
                 fancy-regex: {:?}\n\
                 onig:        {:?}\n",
                d.pattern,
                d.context,
                d.has_action,
                !d.has_action,
                d.line_num,
                d.line,
                d.fancy_captures,
                d.onig_captures,
            );
        }
        panic!(
            "fancy-regex and onig produced different results for {} (pattern, line) pair(s). \
             See the test output above for details.",
            differences.len()
        );
    }
}
