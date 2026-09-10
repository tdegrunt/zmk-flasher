// zmk-flasher: download ZMK firmware from a GitHub Actions run and flash a
// split keyboard over its UF2 bootloader mass-storage volume.

use anyhow::{bail, Context, Result};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Download ZMK firmware from a GitHub Actions run and flash a split
/// keyboard by copying the .uf2 files onto its bootloader volume.
#[derive(Parser, Debug)]
#[command(name = "zmk-flasher", version, about)]
struct Args {
    /// GitHub Actions URL: a workflow page (.../actions/workflows/<file>.yml),
    /// a specific run (.../actions/runs/<id>), or a specific artifact
    /// (.../actions/runs/<id>/artifacts/<artifact_id>)
    #[arg(long, default_value = "https://github.com/tdegrunt/zmk-config/actions/workflows/build.yml")]
    url: String,

    /// Volume name to wait for when flashing the left half
    #[arg(long, default_value = "KEEBART")]
    left_volume: String,

    /// Volume name to wait for when flashing the right half
    #[arg(long, default_value = "KEEBART")]
    right_volume: String,

    /// Firmware filename (inside the artifact zip) for the left half
    #[arg(long, default_value = "nice_view-corne_choc_pro_left-zmk.uf2")]
    left_firmware: String,

    /// Firmware filename (inside the artifact zip) for the right half
    #[arg(long, default_value = "nice_view-corne_choc_pro_right-zmk.uf2")]
    right_firmware: String,

    /// Substring to pick the artifact by name, if a run has more than one
    #[arg(long)]
    artifact_name: Option<String>,

    /// GitHub token (falls back to $GITHUB_TOKEN, then `gh auth token`)
    #[arg(long, env = "GITHUB_TOKEN")]
    token: Option<String>,

    /// Seconds to wait for a volume to appear/disappear before giving up
    #[arg(long, default_value_t = 300)]
    timeout_secs: u64,

    /// Poll interval in milliseconds while waiting for a volume
    #[arg(long, default_value_t = 500)]
    poll_interval_ms: u64,
}

enum Target {
    Workflow(String),
    Run(u64),
    Artifact(u64),
}

struct ArtifactRef {
    id: u64,
    name: String,
    archive_download_url: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let token = resolve_token(&args)?;
    let (owner, repo, target) = parse_github_actions_url(&args.url)?;

    let client = build_client(&token)?;

    println!("Repository: {owner}/{repo}");

    let artifact = match target {
        Target::Artifact(id) => {
            println!("Using explicit artifact id {id}");
            fetch_artifact_by_id(&client, &owner, &repo, id)?
        }
        Target::Run(run_id) => {
            println!("Using run {run_id}");
            pick_artifact(
                &client,
                &owner,
                &repo,
                run_id,
                args.artifact_name.as_deref(),
            )?
        }
        Target::Workflow(workflow) => {
            println!("Looking up the latest run of workflow '{workflow}'...");
            let run_id = latest_run_id(&client, &owner, &repo, &workflow)?;
            println!("Latest run: {run_id}");
            pick_artifact(
                &client,
                &owner,
                &repo,
                run_id,
                args.artifact_name.as_deref(),
            )?
        }
    };

    println!("Artifact: {} (id {})", artifact.name, artifact.id);
    println!("Downloading artifact...");
    let zip_bytes = download_artifact_zip(&client, &artifact)?;
    println!("Downloaded {} bytes", zip_bytes.len());

    let wanted = [args.left_firmware.as_str(), args.right_firmware.as_str()];
    let extracted = extract_firmware(&zip_bytes, &wanted)?;

    let left_path = extracted.get(args.left_firmware.as_str()).with_context(|| {
        format!(
            "Firmware file '{}' was not found in the downloaded artifact",
            args.left_firmware
        )
    })?;
    let right_path = extracted.get(args.right_firmware.as_str()).with_context(|| {
        format!(
            "Firmware file '{}' was not found in the downloaded artifact",
            args.right_firmware
        )
    })?;

    let timeout = Duration::from_secs(args.timeout_secs);
    let poll = Duration::from_millis(args.poll_interval_ms);

    flash_half("LEFT", &args.left_volume, left_path, timeout, poll)?;
    flash_half("RIGHT", &args.right_volume, right_path, timeout, poll)?;

    println!("\nBoth halves flashed successfully.");
    Ok(())
}

fn flash_half(
    label: &str,
    volume_name: &str,
    firmware_path: &Path,
    timeout: Duration,
    poll: Duration,
) -> Result<()> {
    println!("\n==> Double-tap the RESET/bootloader button on the {label} half now.");
    wait_for_condition(
        &format!("Waiting for volume '{volume_name}' to appear..."),
        timeout,
        poll,
        || volume_path(volume_name).is_some(),
    )?;
    let vol_path = volume_path(volume_name)
        .with_context(|| format!("Volume '{volume_name}' disappeared before it could be used"))?;

    let dest = vol_path.join(
        firmware_path
            .file_name()
            .context("firmware path has no file name")?,
    );
    println!(
        "Copying {} -> {}",
        firmware_path.display(),
        dest.display()
    );
    std::fs::copy(firmware_path, &dest).with_context(|| {
        format!(
            "Failed to copy {} to {}",
            firmware_path.display(),
            dest.display()
        )
    })?;
    // Flush filesystem buffers so the write is actually on the device
    // before we start polling for the drive to disappear.
    let _ = Command::new("sync").status();

    wait_for_condition(
        &format!("Waiting for volume '{volume_name}' to disappear (flashing)..."),
        timeout,
        poll,
        || !vol_path.exists(),
    )?;
    println!("{label} half flashed.");
    Ok(())
}

fn resolve_token(args: &Args) -> Result<String> {
    if let Some(t) = &args.token {
        if !t.trim().is_empty() {
            return Ok(t.clone());
        }
    }
    if let Some(t) = token_from_gh_cli() {
        return Ok(t);
    }
    bail!(
        "No GitHub token found. Pass --token, set $GITHUB_TOKEN, or run `gh auth login` \
         (this tool will use `gh auth token` automatically)."
    )
}

fn token_from_gh_cli() -> Option<String> {
    let output = Command::new("gh").args(["auth", "token"]).output().ok()?;
    if output.status.success() {
        let s = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

fn build_client(token: &str) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static("zmk-flasher"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(
        "X-GitHub-Api-Version",
        HeaderValue::from_static("2022-11-28"),
    );
    let auth = HeaderValue::from_str(&format!("Bearer {token}"))
        .context("token contains invalid header characters")?;
    headers.insert(AUTHORIZATION, auth);

    Ok(Client::builder()
        .default_headers(headers)
        .build()
        .context("failed to build HTTP client")?)
}

fn parse_github_actions_url(url_str: &str) -> Result<(String, String, Target)> {
    let url = url::Url::parse(url_str).with_context(|| format!("invalid URL: {url_str}"))?;
    let segments: Vec<&str> = url
        .path_segments()
        .context("URL has no path segments")?
        .filter(|s| !s.is_empty())
        .collect();

    if segments.len() < 4 || segments[2] != "actions" {
        bail!(
            "Expected a GitHub Actions URL like \
             https://github.com/<owner>/<repo>/actions/workflows/<file> or \
             https://github.com/<owner>/<repo>/actions/runs/<id>, got: {url_str}"
        );
    }

    let owner = segments[0].to_string();
    let repo = segments[1].to_string();

    let target = match segments[3] {
        "workflows" => {
            let workflow = segments
                .get(4)
                .context("workflow URL is missing the workflow file name")?;
            Target::Workflow(workflow.to_string())
        }
        "runs" => {
            let run_id: u64 = segments
                .get(4)
                .context("run URL is missing the run id")?
                .parse()
                .context("run id is not a number")?;
            if segments.get(5) == Some(&"artifacts") {
                let artifact_id: u64 = segments
                    .get(6)
                    .context("artifact URL is missing the artifact id")?
                    .parse()
                    .context("artifact id is not a number")?;
                Target::Artifact(artifact_id)
            } else {
                Target::Run(run_id)
            }
        }
        other => bail!("Unsupported Actions URL shape (segment '{other}'): {url_str}"),
    };

    Ok((owner, repo, target))
}

fn get_json(client: &Client, url: &str) -> Result<Value> {
    let resp = client.get(url).send().with_context(|| format!("request to {url} failed"))?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("GitHub API request to {url} failed: {status}\n{text}");
    }
    serde_json::from_str(&text).with_context(|| format!("failed to parse JSON from {url}"))
}

fn latest_run_id(client: &Client, owner: &str, repo: &str, workflow: &str) -> Result<u64> {
    let url = format!(
        "https://api.github.com/repos/{owner}/{repo}/actions/workflows/{workflow}/runs?per_page=1"
    );
    let json = get_json(client, &url)?;
    json["workflow_runs"][0]["id"]
        .as_u64()
        .with_context(|| format!("no runs found for workflow '{workflow}'"))
}

fn list_artifacts(client: &Client, owner: &str, repo: &str, run_id: u64) -> Result<Vec<ArtifactRef>> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/actions/runs/{run_id}/artifacts");
    let json = get_json(client, &url)?;
    let arr = json["artifacts"]
        .as_array()
        .with_context(|| format!("no artifacts field in response for run {run_id}"))?;
    let mut out = Vec::new();
    for a in arr {
        out.push(ArtifactRef {
            id: a["id"].as_u64().context("artifact missing id")?,
            name: a["name"].as_str().unwrap_or("").to_string(),
            archive_download_url: a["archive_download_url"]
                .as_str()
                .context("artifact missing archive_download_url")?
                .to_string(),
        });
    }
    Ok(out)
}

fn fetch_artifact_by_id(client: &Client, owner: &str, repo: &str, id: u64) -> Result<ArtifactRef> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/actions/artifacts/{id}");
    let json = get_json(client, &url)?;
    Ok(ArtifactRef {
        id: json["id"].as_u64().context("artifact missing id")?,
        name: json["name"].as_str().unwrap_or("").to_string(),
        archive_download_url: json["archive_download_url"]
            .as_str()
            .context("artifact missing archive_download_url")?
            .to_string(),
    })
}

fn pick_artifact(
    client: &Client,
    owner: &str,
    repo: &str,
    run_id: u64,
    name_filter: Option<&str>,
) -> Result<ArtifactRef> {
    let artifacts = list_artifacts(client, owner, repo, run_id)?;
    if artifacts.is_empty() {
        bail!("Run {run_id} has no artifacts");
    }

    if let Some(filter) = name_filter {
        return artifacts
            .into_iter()
            .find(|a| a.name.contains(filter))
            .with_context(|| format!("no artifact matching '{filter}' in run {run_id}"));
    }

    if artifacts.len() == 1 {
        return Ok(artifacts.into_iter().next().unwrap());
    }

    let names: Vec<&str> = artifacts.iter().map(|a| a.name.as_str()).collect();
    bail!(
        "Run {run_id} has multiple artifacts ({}); pass --artifact-name to pick one",
        names.join(", ")
    );
}

fn download_artifact_zip(client: &Client, artifact: &ArtifactRef) -> Result<Vec<u8>> {
    let resp = client
        .get(&artifact.archive_download_url)
        .send()
        .context("failed to download artifact")?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().unwrap_or_default();
        bail!("Failed to download artifact '{}': {status}\n{text}", artifact.name);
    }
    Ok(resp.bytes()?.to_vec())
}

fn extract_firmware(zip_bytes: &[u8], wanted: &[&str]) -> Result<HashMap<String, PathBuf>> {
    let reader = Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(reader).context("downloaded artifact is not a valid zip")?;

    let tmp_dir = std::env::temp_dir().join(format!("zmk-flasher-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir)?;

    let mut found = HashMap::new();
    let mut all_names = Vec::new();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        if file.is_dir() {
            continue;
        }
        let name = file.name().to_string();
        let base = Path::new(&name)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(&name)
            .to_string();
        all_names.push(name.clone());

        if wanted.contains(&base.as_str()) {
            let out_path = tmp_dir.join(&base);
            let mut out = std::fs::File::create(&out_path)
                .with_context(|| format!("failed to create {}", out_path.display()))?;
            std::io::copy(&mut file, &mut out)
                .with_context(|| format!("failed to extract {name}"))?;
            found.insert(base, out_path);
        }
    }

    for w in wanted {
        if !found.contains_key(*w) {
            eprintln!("\nFiles present in the artifact zip:");
            for n in &all_names {
                eprintln!("  {n}");
            }
            eprintln!();
        }
    }

    Ok(found)
}

fn volume_path(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from("/Volumes").join(name);
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

fn wait_for_condition<F: Fn() -> bool>(
    message: &str,
    timeout: Duration,
    poll: Duration,
    cond: F,
) -> Result<()> {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner} {msg}")
            .unwrap()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"),
    );
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(120));

    let start = Instant::now();
    loop {
        if cond() {
            pb.finish_and_clear();
            return Ok(());
        }
        if start.elapsed() > timeout {
            pb.finish_and_clear();
            bail!("Timed out after {:?}: {message}", timeout);
        }
        std::thread::sleep(poll);
    }
}
