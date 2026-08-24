// Thin command router. Rendering is the data plane (runtime.rs); config
// mutation is the control plane (management.rs); --refresh-quota is the
// detached cache-refresher spawned off the hot path.

mod git_status;
mod management;
mod metrics;
mod model_config;
mod paths;
mod payload;
mod pr;
mod quota;
mod render;
mod runtime;
mod theme;
mod thinking;
mod toml_edit;
mod util;

use std::path::PathBuf;
use std::process::ExitCode;

use paths::RuntimePaths;

const HELP: &str = r#"kimi-code-hud-rs — custom status line for Kimi Code CLI

Usage:
  kimi-code-hud-rs                  render the status line (reads JSON from stdin)
  kimi-code-hud-rs --install        register in ~/.kimi-code/tui.toml
  kimi-code-hud-rs --uninstall      remove the tui.toml entry
  kimi-code-hud-rs --refresh-quota  refresh the quota cache (internal, silent)
  kimi-code-hud-rs --refresh-pr     refresh the PR badge cache via gh (internal, silent)
  kimi-code-hud-rs --help           show this help

The host may wipe the [status_line] entry on kimi-code upgrades — when the
HUD disappears, simply re-run --install and restart the session (/reload-tui).

Config: ~/.kimi-code-hud-rs/config.json (JSONC — comments and trailing commas allowed)
        {"layout":..., "items":[...],
         "slots": {"<slot>": {"color":"<token|#hex>","bold":bool,"format":"...",
                              "normal":{...}, "compact":{...}}}}
        format: long|short (git/speed/cache/quota) or short|full|name (cwd).
        Mode badges style individually as "auto"/"yolo"/"plan"/"swarm".
Env:    KIMI_HUD_RS_LAYOUT / KIMI_HUD_RS_CWD / KIMI_HUD_RS_ITEMS override the config;
        NO_COLOR / KIMI_HUD_RS_NO_COLOR disable colors.
        KIMI_HUD_RS_THEME=dark|light pins the badge palette (default: tui.toml's
        theme, with auto resolved via COLORFGBG, falling back to dark).
"#;

fn admin_failure(action: &str, err: &str) -> ExitCode {
    eprintln!("kimi-code-hud-rs: {} failed: {}", action, err);
    ExitCode::FAILURE
}

/// Render path: fail-open. A panic degrades to the fallback line instead of
/// leaking a diagnostic into the status bar.
fn render_main(paths: &RuntimePaths) -> ExitCode {
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(|| runtime::render_status_line(paths));
    let line = match result {
        Ok((code, Some(line))) => {
            println!("{}", line);
            return if code == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE };
        }
        Ok((_, None)) => String::new(),
        Err(_) => "kimi-code-hud-rs".to_string(),
    };
    println!("{}", line);
    ExitCode::SUCCESS
}

fn refresh_quota_main(paths: &RuntimePaths) -> ExitCode {
    // Region resolution reads config.toml here only — the detached refresh is
    // off the render hot path.
    let endpoints = quota::resolve_quota_endpoints(
        std::env::var("KIMI_CODE_OAUTH_HOST").ok().as_deref(),
        std::env::var("KIMI_OAUTH_HOST").ok().or(std::env::var("KIMI_CODE_BASE_URL").ok()).as_deref(),
        &util::read_string(&paths.config_toml_path).unwrap_or_default(),
        &paths.kimi_home,
    );
    // Detached refresh is silent by contract.
    let _ = quota::refresh_quota(
        &endpoints,
        &paths.quota_cache_path,
        &paths.quota_lock_path,
        std::env::var("KIMI_HUD_RS_QUOTA_LOCK_TOKEN").ok().as_deref(),
    );
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", HELP);
        return ExitCode::SUCCESS;
    }
    let paths = RuntimePaths::resolve();
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("kimi-code-hud-rs"));

    if args.contains(&"--refresh-quota".to_string()) {
        return refresh_quota_main(&paths);
    }
    if args.contains(&"--refresh-pr".to_string()) {
        let cwd = std::env::var("KIMI_HUD_RS_PR_CWD").unwrap_or_default();
        let branch = std::env::var("KIMI_HUD_RS_PR_BRANCH").unwrap_or_default();
        if !cwd.is_empty() && !branch.is_empty() {
            pr::refresh_pr(
                &cwd,
                &branch,
                &paths.pr_cache_path,
                &paths.pr_lock_path,
                std::env::var("KIMI_HUD_RS_PR_LOCK_TOKEN").ok().as_deref(),
            );
        }
        return ExitCode::SUCCESS;
    }
    let actions: [(&str, &str, fn(&std::path::Path, &RuntimePaths) -> Result<(), String>); 2] = [
        ("--install", "install", management::install),
        ("--uninstall", "uninstall", management::uninstall),
    ];
    for (flag, name, action) in actions {
        if args.iter().any(|a| a == flag) {
            return match action(&exe, &paths) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => admin_failure(name, &err),
            };
        }
    }
    render_main(&paths)
}
