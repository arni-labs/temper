//! Which of an app's files make each part of its bundle: its specs, CSDL,
//! policies, WASM modules, agents, skills, ADRs and seed data. Each part is
//! picked from the app's [`AppFiles`] and read by its path.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use super::app_files::{AppFiles, join};
use super::{
    AdrEntry, AgentDefinition, AppSkillDefinition, CompanionFile, SeedFile, SeedInstance,
    WasmModuleManifest, extract_description,
};

/// Every IOA spec of the app: `*.ioa.toml` in its own folder first, then in
/// `specs/`. A name in both is taken from the app's own folder.
pub(crate) fn find_ioa_files(files: &AppFiles) -> Vec<String> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for folder in ["", "specs"] {
        for name in files.files_in(folder) {
            if name.ends_with(".ioa.toml") && seen.insert(name.clone()) {
                found.push(join(folder, &name));
            }
        }
    }
    found
}

/// The CSDL model: `model.csdl.xml`, then `specs/model.csdl.xml`, then the
/// first `csdl/*.csdl.xml`.
pub(super) fn find_csdl(files: &AppFiles) -> Option<String> {
    ["model.csdl.xml", "specs/model.csdl.xml"]
        .into_iter()
        .find(|path| files.has(path))
        .map(str::to_string)
        .or_else(|| {
            files
                .files_in("csdl")
                .into_iter()
                .find(|name| name.ends_with(".csdl.xml"))
                .map(|name| format!("csdl/{name}"))
        })
}

/// The Cedar policies directly in `folder`, sorted.
pub(super) fn find_cedar_policies(files: &AppFiles, folder: &str) -> Vec<String> {
    files
        .files_in(folder)
        .into_iter()
        .filter(|name| name.ends_with(".cedar"))
        .map(|name| format!("{folder}/{name}"))
        .collect()
}

/// The compiled WASM module of every module `app.toml` declares.
///
/// Prefers packaged artifacts copied next to the module, then
/// manifest-selected target outputs and common WASM release outputs.
pub(super) fn find_wasm_modules(
    files: &AppFiles,
    module_configs: &BTreeMap<String, WasmModuleManifest>,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut modules = BTreeMap::new();
    if !files.has_dir("wasm") {
        return Ok(modules);
    }

    for (module_name, config) in module_configs {
        let module_dir = format!("wasm/{module_name}");
        if !files.has_dir(&module_dir) {
            continue;
        }
        // Prefer packaged sibling artifacts because they are the deployable
        // copy left after Docker target pruning. Target output remains a local
        // development fallback.
        let dashed_name = module_name.replace('_', "-");
        let mut candidates = vec![
            format!("{module_dir}/{module_name}.wasm"),
            format!("{module_dir}/{dashed_name}.wasm"),
            format!("wasm/{module_name}.wasm"),
            format!("wasm/{dashed_name}.wasm"),
        ];
        let targets = match config.target.as_deref() {
            Some(target) => vec![target],
            None => vec!["wasm32-unknown-unknown", "wasm32-wasip1"],
        };
        for name in [module_name.as_str(), dashed_name.as_str()] {
            for target in &targets {
                candidates.push(format!("{module_dir}/target/{target}/release/{name}.wasm"));
            }
        }

        if let Some(path) = candidates.iter().find(|path| files.has(path)) {
            let bytes = files.read(path)?;
            tracing::debug!(
                module = %module_name,
                path = %path,
                size = bytes.len(),
                "Found WASM module in OS app"
            );
            modules.insert(module_name.clone(), bytes);
        }
    }

    warn_undeclared_wasm_artifacts(files, module_configs);
    Ok(modules)
}

fn warn_undeclared_wasm_artifacts(
    files: &AppFiles,
    module_configs: &BTreeMap<String, WasmModuleManifest>,
) {
    for module_name in files.dirs_in("wasm") {
        if module_name == "target" || module_configs.contains_key(&module_name) {
            continue;
        }
        let sibling = format!("wasm/{module_name}/{module_name}.wasm");
        if files.has(&sibling) {
            tracing::warn!(
                module = %module_name,
                path = %sibling,
                "Ignoring undeclared bundled WASM artifact; declare it in app.toml [[wasm_modules]] to install it"
            );
        }
    }
}

/// Agent definitions from `agents/{name}/`.
///
/// Each folder is one agent. Its `.md` files are concatenated in name order.
/// The platform is filename-agnostic: conventions like SOUL.md, STYLE.md and
/// AGENT.md are for humans.
pub(super) fn find_agents(files: &AppFiles) -> Result<Vec<AgentDefinition>, String> {
    let mut results = Vec::new();
    for agent_name in files.dirs_in("agents") {
        let agent_dir = format!("agents/{agent_name}");
        let md_files: Vec<String> = files
            .files_in(&agent_dir)
            .into_iter()
            .filter(|name| name.to_lowercase().ends_with(".md"))
            .collect();
        if md_files.is_empty() {
            continue;
        }
        let has_soul = md_files.iter().any(|name| name == "SOUL.md");

        let mut content = String::new();
        for name in &md_files {
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(&files.read_text(&format!("{agent_dir}/{name}"))?);
        }

        let description =
            extract_description(&content).unwrap_or_else(|| format!("Agent: {agent_name}"));
        results.push(AgentDefinition {
            name: agent_name,
            content,
            has_soul,
            description,
        });
    }
    Ok(results)
}

/// Skill definitions: `system/skills/{name}/` (system-level) and
/// `agents/{agent}/skills/{name}/` (agent-scoped).
///
/// A skill's folder holds its `SKILL.md`; every other file in it is a
/// companion file. A folder without a `SKILL.md` is not a skill.
pub(super) fn find_app_skills(files: &AppFiles) -> Result<Vec<AppSkillDefinition>, String> {
    let mut results = Vec::new();
    scan_skill_dirs(files, "system/skills", None, &mut results)?;
    for agent_name in files.dirs_in("agents") {
        let skills_dir = format!("agents/{agent_name}/skills");
        scan_skill_dirs(files, &skills_dir, Some(&agent_name), &mut results)?;
    }
    Ok(results)
}

fn scan_skill_dirs(
    files: &AppFiles,
    skills_dir: &str,
    agent_name: Option<&str>,
    results: &mut Vec<AppSkillDefinition>,
) -> Result<(), String> {
    for skill_name in files.dirs_in(skills_dir) {
        let skill_dir = format!("{skills_dir}/{skill_name}");
        let skill_path = format!("{skill_dir}/SKILL.md");
        if !files.has(&skill_path) {
            continue;
        }
        let content = files.read_text(&skill_path)?;
        let frontmatter = super::extract_frontmatter(&content);
        let description = frontmatter
            .description
            .or_else(|| extract_description(&content))
            .unwrap_or_else(|| format!("Skill: {skill_name}"));

        let mut companion_files = Vec::new();
        for name in files.files_under(&skill_dir) {
            if Path::new(&name)
                .file_name()
                .is_some_and(|f| f == "SKILL.md")
            {
                continue;
            }
            companion_files.push(CompanionFile {
                content: files.read(&format!("{skill_dir}/{name}"))?,
                mime_type: super::mime_from_extension(Path::new(&name)),
                name,
            });
        }

        results.push(AppSkillDefinition {
            name: skill_name,
            content,
            description,
            agent_name: agent_name.map(String::from),
            companion_files,
        });
    }
    Ok(())
}

/// App-local ADRs from `adrs/*.md`, in name order.
pub(super) fn find_adrs(files: &AppFiles) -> Result<Vec<AdrEntry>, String> {
    let mut results = Vec::new();
    for file_name in files.files_in("adrs") {
        let path = Path::new(&file_name);
        let is_markdown = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
        if !is_markdown {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(&file_name)
            .to_string();
        results.push(AdrEntry {
            name,
            content: files.read_text(&format!("adrs/{file_name}"))?,
            file_name,
        });
    }
    Ok(results)
}

/// Seed data instances from `seed-data/*.toml`, each holding `[[instance]]`
/// blocks of entities to create on first install. A file that does not
/// parse is left out with a warning.
pub(super) fn find_seed_data(files: &AppFiles) -> Result<Vec<SeedInstance>, String> {
    let mut all_instances = Vec::new();
    for name in files.files_in("seed-data") {
        if !name.ends_with(".toml") {
            continue;
        }
        let path = format!("seed-data/{name}");
        match toml::from_str::<SeedFile>(&files.read_text(&path)?) {
            Ok(seed_file) => all_instances.extend(seed_file.instances),
            Err(e) => {
                tracing::warn!(path = %path, error = %e, "Failed to parse seed data file");
            }
        }
    }
    Ok(all_instances)
}
