//! Offline Agentfile compiler. Only explicit inputs may become agent configuration.
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{
    Deserialize,
    de::{DeserializeOwned, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::types::{
    BUILTIN_ABI, Bundle, BundleFile, Manifest, McpSpec, PTC_ABI, PromptSpec, SkillSpec, ValueRef,
};

const FORMAT: &str = "aporto.bundle";
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_BUNDLE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_FILES: usize = 10_000;
const MAX_SCANNED_ENTRIES: usize = 20_000;
struct BuildRoot {
    path: PathBuf,
    ignored: GlobSet,
}
struct Snapshot {
    roots: BTreeMap<String, BuildRoot>,
    files: BTreeMap<String, BundleFile>,
    total: usize,
    scanned_entries: usize,
}

impl BuildRoot {
    fn new(path: &Path) -> Result<Self> {
        let path = fs::canonicalize(path).context("open build context")?;
        ensure!(path.is_dir(), "build context must be a directory");
        let mut patterns = GlobSetBuilder::new();
        let ignore_path = path.join(".agentignore");
        if let Ok(meta) = fs::symlink_metadata(&ignore_path) {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                ".agentignore must be a regular file"
            );
            ensure!(meta.len() <= 1024 * 1024, ".agentignore is too large");
            let ignore = String::from_utf8(read_limited(&ignore_path, 1024 * 1024)?)?;
            for line in ignore.lines() {
                let p = line.trim();
                if p.is_empty() || p.starts_with('#') {
                    continue;
                }
                ensure!(
                    !p.starts_with('!'),
                    ".agentignore does not support negation"
                );
                ensure!(
                    !p.contains('\\') && !p.split('/').any(|x| x == ".."),
                    "invalid ignore pattern"
                );
                let p = p.trim_start_matches('/').trim_end_matches('/');
                ensure!(!p.is_empty(), "empty ignore pattern");
                let forms = if p.contains('/') {
                    vec![p.to_string(), format!("{p}/**")]
                } else {
                    vec![format!("**/{p}"), format!("**/{p}/**")]
                };
                for form in forms {
                    patterns.add(
                        GlobBuilder::new(&form)
                            .literal_separator(true)
                            .build()
                            .context("invalid ignore glob")?,
                    );
                }
            }
        }
        Ok(Self {
            path,
            ignored: patterns.build()?,
        })
    }

    fn excluded(&self, relative: &str) -> bool {
        let mut parent = String::new();
        relative.split('/').any(|part| {
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(part);
            let lower = part.to_ascii_lowercase();
            let sensitive = matches!(
                lower.as_str(),
                ".git"
                    | ".ssh"
                    | ".aws"
                    | ".azure"
                    | ".kube"
                    | ".env"
                    | ".netrc"
                    | ".npmrc"
                    | ".pypirc"
                    | "credentials"
                    | "credentials.json"
                    | "id_rsa"
                    | "id_ed25519"
                    | "id_ecdsa"
                    | "id_dsa"
                    | "auth.json"
            ) || lower.starts_with(".env.")
                || [".pem", ".key", ".p12", ".pfx"]
                    .iter()
                    .any(|ext| lower.ends_with(ext));
            sensitive || self.ignored.is_match(&parent)
        })
    }

    fn checked_path(&self, relative: &str) -> Result<PathBuf> {
        ensure!(
            !self.excluded(relative),
            "source is excluded by credential or .agentignore rules: {relative}"
        );
        let mut path = self.path.clone();
        for part in relative.split('/').filter(|p| *p != ".") {
            path.push(part);
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("read declared source {relative}"))?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "symbolic links are not allowed: {relative}"
            );
            ensure!(
                metadata.is_file() || metadata.is_dir(),
                "special files are not allowed: {relative}"
            );
        }
        Ok(path)
    }
}

impl Snapshot {
    fn account_scan(&mut self) -> Result<()> {
        ensure!(
            self.scanned_entries < MAX_SCANNED_ENTRIES,
            "build context scan exceeds {MAX_SCANNED_ENTRIES} entries"
        );
        self.scanned_entries += 1;
        Ok(())
    }

    fn source(&self, source: &str) -> Result<(String, String)> {
        let (root, relative) = if let Some(s) = source.strip_prefix('@') {
            let (name, path) = s
                .split_once('/')
                .context("named source must be @name/path")?;
            identifier(name)?;
            (name.to_string(), path.to_string())
        } else {
            (String::new(), source.to_string())
        };
        relative_path(&relative, true)?;
        ensure!(
            self.roots.contains_key(&root),
            "undeclared named build context: {root}"
        );
        Ok((root, relative))
    }

    fn copy(&mut self, source: &str, target: &str, require_dir: bool) -> Result<()> {
        self.account_scan()?;
        relative_path(target, false)?;
        let (root, relative) = self.source(source)?;
        let path = self.roots[&root].checked_path(&relative)?;
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            !require_dir || metadata.is_dir(),
            "source must be a directory: {source}"
        );
        let before = self.files.len();
        self.visit(&root, &relative, target, 0)?;
        ensure!(
            self.files.len() > before,
            "declared source contains no included files: {source}"
        );
        Ok(())
    }

    fn visit(&mut self, root: &str, relative: &str, target: &str, depth: usize) -> Result<()> {
        ensure!(depth < 64, "source directory nesting exceeds limit");
        let ctx = &self.roots[root];
        let path = ctx.checked_path(relative)?;
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            let mut entries = Vec::new();
            for entry in fs::read_dir(path)? {
                // Count ignored entries and empty directories too, before buffering
                // them for deterministic sorting or recursively visiting children.
                self.account_scan()?;
                entries.push(entry?);
            }
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("file names must be UTF-8"))?;
                let child = if relative == "." {
                    name.clone()
                } else {
                    format!("{relative}/{name}")
                };
                relative_path(&child, false)?;
                if !self.roots[root].excluded(&child) {
                    self.visit(root, &child, &format!("{target}/{name}"), depth + 1)?;
                }
            }
        } else {
            ensure!(metadata.is_file(), "only regular files may be included");
            ensure!(
                metadata.len() <= MAX_FILE_BYTES as u64,
                "file exceeds 8 MiB: {relative}"
            );
            let data = read_limited(&path, MAX_FILE_BYTES)?;
            ensure!(
                data.len() <= MAX_FILE_BYTES,
                "file exceeds 8 MiB: {relative}"
            );
            self.total = self
                .total
                .checked_add(data.len())
                .context("bundle size overflow")?;
            ensure!(
                self.total <= MAX_TOTAL_BYTES,
                "bundle contents exceed 64 MiB"
            );
            ensure!(
                self.files.len() < MAX_FILES,
                "bundle exceeds file count limit"
            );
            ensure!(
                !self.files.contains_key(target),
                "duplicate bundle target: {target}"
            );
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            self.files.insert(
                target.to_string(),
                BundleFile {
                    sha256: hex_hash(&data),
                    data: STANDARD.encode(data),
                    mode: if executable { 0o755 } else { 0o644 },
                },
            );
        }
        Ok(())
    }
}

/// Compile a context without inspecting runtime or user-global configuration.
pub fn build(
    context: &Path,
    agentfile: &Path,
    named_contexts: &BTreeMap<String, PathBuf>,
) -> Result<Bundle> {
    let mut roots = BTreeMap::new();
    roots.insert(String::new(), BuildRoot::new(context)?);
    for (name, path) in named_contexts {
        identifier(name)?;
        roots.insert(name.clone(), BuildRoot::new(path)?);
    }
    let agentfile = if agentfile.is_absolute() {
        agentfile.to_path_buf()
    } else {
        roots[""].path.join(agentfile)
    };
    let metadata = fs::symlink_metadata(&agentfile).context("read Agentfile")?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Agentfile must be a regular file"
    );
    ensure!(metadata.len() <= 1024 * 1024, "Agentfile exceeds 1 MiB");
    let instructions = String::from_utf8(read_limited(&agentfile, 1024 * 1024)?)
        .context("Agentfile must be UTF-8")?;
    let mut snapshot = Snapshot {
        roots,
        files: BTreeMap::new(),
        total: 0,
        scanned_entries: 0,
    };
    let input = crate::agentfile::parse(&instructions)?;
    let mut prompts = Vec::new();
    let mut skills = Vec::new();
    let mut mcp = Vec::new();
    let mut secrets = BTreeSet::new();
    for input in input.prompts {
        let path = format!("prompts/{}.md", prompts.len());
        let (root, relative) = snapshot.source(&input.source)?;
        ensure!(
            snapshot.roots[&root].checked_path(&relative)?.is_file(),
            "prompt source must be a file"
        );
        snapshot.copy(&input.source, &path, false)?;
        prompts.push(PromptSpec {
            role: input.role.as_str().to_owned(),
            path,
        });
    }
    for input in input.skills {
        let (root, relative) = snapshot.source(&input.source)?;
        let entry = if relative == "." {
            "SKILL.md".to_owned()
        } else {
            format!("{relative}/SKILL.md")
        };
        let entry_path = snapshot.roots[&root].checked_path(&entry)?;
        ensure!(
            fs::metadata(&entry_path)?.len() <= MAX_FILE_BYTES as u64,
            "SKILL.md exceeds size limit"
        );
        let entry_text = String::from_utf8(read_limited(&entry_path, MAX_FILE_BYTES)?)
            .context("SKILL.md must be UTF-8")?;
        let (name, description) = skill_metadata(&entry_text)?;
        ensure!(
            !skills.iter().any(|s: &SkillSpec| s.name == name),
            "duplicate skill name: {name}"
        );
        snapshot.copy(&input.source, &format!("skills/{name}"), true)?;
        skills.push(SkillSpec {
            path: format!("skills/{name}/SKILL.md"),
            name,
            description,
        });
    }
    for input in input.mcp {
        identifier(&input.name)?;
        ensure!(
            !mcp.iter().any(|m: &McpSpec| m.name == input.name),
            "duplicate MCP name"
        );
        let transport = if input.command.is_some() {
            "stdio"
        } else {
            "http"
        }
        .to_owned();
        let path = if let Some(source) = input.source {
            let path = format!("mcp/{}", input.name);
            snapshot.copy(&source, &path, true)?;
            Some(path)
        } else {
            None
        };
        let env = input.env.unwrap_or_default();
        let headers = input.headers.unwrap_or_default();
        for value in env.values().chain(headers.values()) {
            if let ValueRef::Secret { secret } = value {
                secrets.insert(secret.clone());
            }
        }
        mcp.push(McpSpec {
            name: input.name,
            transport,
            path,
            command: input.command,
            url: input.url,
            env,
            headers,
            include_tools: input.include_tools,
        });
    }
    for input in input.assets {
        ensure!(
            input.target.starts_with("assets/"),
            "asset target must be below assets/"
        );
        snapshot.copy(&input.source, &input.target, false)?;
    }
    let mut bundle = Bundle {
        format: FORMAT.to_string(),
        digest: String::new(),
        files: snapshot.files,
        manifest: Manifest {
            name: input.agent.name,
            description: input.agent.description,
            model: input.model,
            runtime: input.runtime,
            builtin_abi: BUILTIN_ABI.to_owned(),
            ptc_abi: PTC_ABI.to_owned(),
            prompts,
            skills,
            mcp,
            secrets: secrets.into_iter().collect(),
            limits: input.limits.resolve(),
        },
    };
    bundle.digest = bundle_digest(&bundle)?;
    verify_bundle(&bundle)?;
    Ok(bundle)
}

/// Hash the canonical JSON payload, excluding the digest field itself.
fn bundle_digest(bundle: &Bundle) -> Result<String> {
    let value = serde_json::json!({"format": bundle.format, "manifest": bundle.manifest, "files": bundle.files});
    Ok(format!(
        "sha256:{}",
        hex_hash(canonical_json(&value).as_bytes())
    ))
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(k, v)| format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap(),
                        canonical_json(v)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => value.to_string(),
    }
}

fn hex_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn identifier(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name.bytes().next().unwrap().is_ascii_alphanumeric()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "invalid name: {name}"
    );
    Ok(())
}

fn secret_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && !name.as_bytes()[0].is_ascii_digit()
            && name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
        "invalid secret name"
    );
    Ok(())
}

fn relative_path(path: &str, root_allowed: bool) -> Result<()> {
    if root_allowed && path == "." {
        return Ok(());
    }
    ensure!(
        !path.is_empty()
            && path.len() <= 1024
            && !path.contains('\\')
            && !path.contains(':')
            && !path.chars().any(char::is_control),
        "invalid relative path"
    );
    ensure!(
        !path.starts_with('/')
            && !path.ends_with('/')
            && !path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == ".."),
        "path must stay within its context: {path}"
    );
    ensure!(
        Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "path must be relative"
    );
    Ok(())
}

fn skill_metadata(markdown: &str) -> Result<(String, String)> {
    let normalized = markdown.replace("\r\n", "\n");
    let body = normalized
        .strip_prefix("---\n")
        .context("SKILL.md requires YAML frontmatter")?;
    let mut lines = Vec::new();
    let mut terminated = false;
    for line in body.lines() {
        if line == "---" {
            terminated = true;
            break;
        }
        lines.push(line);
    }
    ensure!(terminated, "unterminated skill frontmatter");
    let value: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&lines.join("\n")).context("invalid skill YAML")?;
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .context("skill frontmatter requires name")?
        .to_string();
    let description = value
        .get("description")
        .and_then(|v| v.as_str())
        .context("skill frontmatter requires description")?
        .to_string();
    identifier(&name)?;
    ensure!(
        !description.trim().is_empty() && description.len() <= 4096,
        "skill description must contain 1..4096 bytes"
    );
    Ok((name, description))
}

fn unique(items: &[String], kind: &str) -> Result<()> {
    ensure!(
        items.iter().collect::<BTreeSet<_>>().len() == items.len(),
        "duplicate {kind}"
    );
    Ok(())
}

fn validate_secret_ref(value: &ValueRef, secrets: &BTreeSet<&str>) -> Result<bool> {
    match value {
        ValueRef::Secret { secret } => {
            secret_name(secret)?;
            ensure!(
                secrets.contains(secret.as_str()),
                "reference to undeclared secret: {secret}"
            );
            Ok(true)
        }
        ValueRef::Literal(value) => {
            ensure!(!value.contains('\0'), "values cannot contain NUL");
            Ok(false)
        }
    }
}

/// Verify structural, path, authorization and content integrity before deployment.
pub fn verify_bundle(bundle: &Bundle) -> Result<()> {
    ensure!(bundle.format == FORMAT, "unsupported bundle format");
    let manifest = &bundle.manifest;
    ensure!(
        manifest.builtin_abi == BUILTIN_ABI && manifest.ptc_abi == PTC_ABI,
        "unsupported builtin or PTC ABI"
    );
    identifier(&manifest.name)?;
    identifier(&manifest.model.connection)?;
    crate::agentfile::validate_model(&manifest.model)?;
    crate::agentfile::validate_runtime(&manifest.runtime)?;
    crate::agentfile::validate_limits(&manifest.limits)?;
    unique(&manifest.secrets, "secret")?;
    ensure!(
        manifest.secrets.windows(2).all(|pair| pair[0] < pair[1]),
        "secret dependencies must be sorted"
    );
    for name in &manifest.secrets {
        secret_name(name)?;
    }
    let secrets = manifest
        .secrets
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    ensure!(bundle.files.len() <= MAX_FILES, "too many bundle files");
    let mut bytes = BTreeMap::new();
    let mut total = 0usize;
    for (path, file) in &bundle.files {
        relative_path(path, false)?;
        ensure!(
            file.mode == 0o644 || file.mode == 0o755,
            "invalid file mode"
        );
        ensure!(
            file.data.len() <= MAX_FILE_BYTES.div_ceil(3) * 4,
            "encoded file exceeds size limit"
        );
        let data = STANDARD.decode(&file.data).context("invalid base64 file")?;
        ensure!(
            STANDARD.encode(&data) == file.data,
            "non-canonical base64 file"
        );
        ensure!(data.len() <= MAX_FILE_BYTES, "file exceeds size limit");
        total = total
            .checked_add(data.len())
            .context("bundle size overflow")?;
        ensure!(
            total <= MAX_TOTAL_BYTES,
            "bundle contents exceed size limit"
        );
        ensure!(
            hex_hash(&data) == file.sha256,
            "file checksum mismatch: {path}"
        );
        let mut ancestor = path.as_str();
        while let Some((parent, _)) = ancestor.rsplit_once('/') {
            ensure!(
                !bundle.files.contains_key(parent),
                "file/directory target conflict: {path}"
            );
            ancestor = parent;
        }
        bytes.insert(path.as_str(), data);
    }
    let mut prompt_paths = BTreeSet::new();
    for (index, prompt) in manifest.prompts.iter().enumerate() {
        ensure!(
            matches!(prompt.role.as_str(), "system" | "developer"),
            "invalid prompt role"
        );
        ensure!(
            prompt.path == format!("prompts/{index}.md"),
            "non-canonical prompt path"
        );
        let data = bytes
            .get(prompt.path.as_str())
            .context("missing prompt file")?;
        std::str::from_utf8(data).context("prompt must be UTF-8")?;
        prompt_paths.insert(prompt.path.as_str());
    }
    let mut skill_names = BTreeSet::new();
    for skill in &manifest.skills {
        identifier(&skill.name)?;
        ensure!(
            skill_names.insert(skill.name.as_str()),
            "duplicate skill name"
        );
        ensure!(
            skill.path == format!("skills/{}/SKILL.md", skill.name),
            "non-canonical skill path"
        );
        let markdown =
            std::str::from_utf8(bytes.get(skill.path.as_str()).context("missing SKILL.md")?)
                .context("skill must be UTF-8")?;
        let (name, description) = skill_metadata(markdown)?;
        ensure!(
            name == skill.name && description == skill.description,
            "skill manifest disagrees with frontmatter"
        );
    }
    let mut mcp_names = BTreeSet::new();
    let mut mcp_paths = BTreeSet::new();
    for mcp in &manifest.mcp {
        identifier(&mcp.name)?;
        ensure!(mcp_names.insert(mcp.name.as_str()), "duplicate MCP name");
        if let Some(tools) = &mcp.include_tools {
            ensure!(!tools.is_empty(), "MCP include_tools must not be empty");
            unique(tools, "MCP tool")?;
            for tool in tools {
                ensure!(
                    !tool.is_empty()
                        && tool.len() <= 128
                        && tool
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.-/".contains(&b)),
                    "invalid MCP tool name or wildcard"
                );
            }
        }
        match mcp.transport.as_str() {
            "stdio" => {
                ensure!(
                    mcp.url.is_none() && mcp.headers.is_empty(),
                    "stdio MCP cannot have URL or headers"
                );
                let command = mcp.command.as_ref().context("stdio MCP requires command")?;
                ensure!(
                    !command.is_empty()
                        && !command[0].trim().is_empty()
                        && command.iter().all(|a| !a.contains('\0')),
                    "invalid MCP command"
                );
                if let Some(path) = &mcp.path {
                    ensure!(
                        path == &format!("mcp/{}", mcp.name),
                        "non-canonical MCP source path"
                    );
                    ensure!(
                        bundle
                            .files
                            .keys()
                            .any(|f| f.starts_with(&format!("{path}/"))),
                        "MCP source has no files"
                    );
                    mcp_paths.insert(path.as_str());
                }
            }
            "http" => {
                ensure!(
                    mcp.command.is_none() && mcp.path.is_none() && mcp.env.is_empty(),
                    "HTTP MCP cannot have command, source or env"
                );
                let url = url::Url::parse(mcp.url.as_deref().context("HTTP MCP requires URL")?)?;
                ensure!(
                    matches!(url.scheme(), "https" | "http")
                        && url.host_str().is_some()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.query().is_none()
                        && url.fragment().is_none(),
                    "invalid MCP URL or embedded credentials/query"
                );
            }
            _ => bail!("unknown MCP transport"),
        }
        for (key, value) in &mcp.env {
            ensure!(
                !key.is_empty()
                    && key.len() <= 128
                    && !key.as_bytes()[0].is_ascii_digit()
                    && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "invalid env name"
            );
            let is_secret = validate_secret_ref(value, &secrets)?;
            let upper = key.to_ascii_uppercase();
            let sensitive = [
                "TOKEN",
                "SECRET",
                "PASSWORD",
                "PASSWD",
                "API_KEY",
                "PRIVATE_KEY",
                "CREDENTIAL",
                "AUTH",
            ]
            .iter()
            .any(|word| upper.contains(word));
            ensure!(
                !sensitive || is_secret,
                "credential environment values must use secret references"
            );
        }
        let mut header_names = BTreeSet::new();
        for (key, value) in &mcp.headers {
            ensure!(
                !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "invalid header name"
            );
            ensure!(
                header_names.insert(key.to_ascii_lowercase()),
                "duplicate case-insensitive MCP header name"
            );
            ensure!(
                !matches!(
                    key.to_ascii_lowercase().as_str(),
                    "host"
                        | "content-length"
                        | "content-type"
                        | "accept"
                        | "transfer-encoding"
                        | "connection"
                        | "mcp-session-id"
                        | "mcp-protocol-version"
                ),
                "reserved MCP header name"
            );
            ensure!(
                validate_secret_ref(value, &secrets)?,
                "MCP headers must use secret references"
            );
        }
    }
    let referenced = manifest
        .mcp
        .iter()
        .flat_map(|mcp| mcp.env.values().chain(mcp.headers.values()))
        .filter_map(|value| match value {
            ValueRef::Secret { secret } => Some(secret.as_str()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    ensure!(
        referenced == secrets,
        "secret dependencies do not match MCP references"
    );
    for path in bundle.files.keys() {
        let declared = path.starts_with("assets/")
            || prompt_paths.contains(path.as_str())
            || skill_names
                .iter()
                .any(|name| path.starts_with(&format!("skills/{name}/")))
            || mcp_paths
                .iter()
                .any(|prefix| path.starts_with(&format!("{prefix}/")));
        ensure!(declared, "file outside declared bundle namespace: {path}");
    }
    ensure!(
        bundle.digest == bundle_digest(bundle)?,
        "bundle checksum mismatch"
    );
    Ok(())
}

/// Parse an untrusted bundle with duplicate-key rejection before semantic verification.
pub fn read_bundle(path: &Path) -> Result<Bundle> {
    let metadata = fs::metadata(path)?;
    ensure!(metadata.is_file(), "bundle must be a regular file");
    ensure!(
        metadata.len() <= MAX_BUNDLE_BYTES,
        "bundle JSON exceeds 100 MiB"
    );
    let data = String::from_utf8(read_limited(path, MAX_BUNDLE_BYTES as usize)?)
        .context("bundle must be UTF-8 JSON")?;
    ensure!(
        data.len() as u64 <= MAX_BUNDLE_BYTES,
        "bundle JSON exceeds 100 MiB"
    );
    let bundle = parse_json(&data)?;
    verify_bundle(&bundle)?;
    Ok(bundle)
}

fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "file exceeds read size limit");
    Ok(bytes)
}

// serde_json::Value and map deserialization ordinarily keep the last duplicate key.
// Reject duplicates at every level before converting to typed schema structures.
struct UniqueValue(Value);
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = UniqueValue;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON with unique object keys")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Number(
                    serde_json::Number::from_f64(v)
                        .ok_or_else(|| E::custom("nonfinite JSON number"))?,
                )))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                v: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                self.visit_unit()
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = a.next_element::<UniqueValue>()? {
                    values.push(value.0);
                }
                Ok(UniqueValue(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Map::new();
                while let Some(key) = a.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON key: {key}"
                        )));
                    }
                    values.insert(key, a.next_value::<UniqueValue>()?.0);
                }
                Ok(UniqueValue(Value::Object(values)))
            }
        }
        d.deserialize_any(UniqueVisitor)
    }
}

fn parse_json<T: DeserializeOwned>(data: &str) -> Result<T> {
    let value = serde_json::from_str::<UniqueValue>(data).context("invalid JSON")?;
    serde_json::from_value(value.0).context("invalid instruction or bundle schema")
}
