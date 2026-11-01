//! MCP prompts: built-in production workflows that name the exact tools in order, plus every saved
//! recipe as a prompt named `recipe:<name>` whose arguments are the recipe's variables.

use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::{
    app::App,
    templates::recipes,
};

const RECIPE_PREFIX: &str = "recipe:";

#[derive(Serialize, JsonSchema)]
pub struct PromptInfo {
    pub name: String,
    pub description: String,
    pub arguments: Vec<PromptArg>,
}

#[derive(Serialize, JsonSchema)]
pub struct PromptArg {
    pub name: String,
    pub description: String,
    pub required: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct PromptText {
    pub name: String,
    pub description: String,
    /// The instructions, as one user message.
    pub text: String,
}

struct Builtin {
    name: &'static str,
    description: &'static str,
    /// (name, description, required)
    args: &'static [(&'static str, &'static str, bool)],
    render: fn(&Args) -> String,
}

struct Args<'a>(&'a Map<String, Value>);

impl Args<'_> {
    fn get(&self, name: &str, fallback: &str) -> String {
        match self.0.get(name) {
            Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
            Some(Value::String(_)) | Some(Value::Null) | None => fallback.to_owned(),
            Some(other) => other.to_string(),
        }
    }
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "product_shoot",
        description: "Product stills with the product locked by a reference: every shot uses the same product images and description.",
        args: &[
            ("product", "Reference name of the product (kind product), or a description if it does not exist yet", true),
            ("scenes", "Comma-separated scenes, e.g. \"marble counter, beach at noon\"", false),
            ("collection", "Collection to file the shots into", false),
        ],
        render: product_shoot,
    },
    Builtin {
        name: "ugc_ad",
        description: "UGC-style ad: a persona holds the product in a still, the still becomes a talking handheld video.",
        args: &[
            ("product", "Reference name of the product", true),
            ("persona", "Reference name of the creator character, or a description", false),
            ("script", "What the persona says (hook first)", false),
            ("aspect", "Aspect ratio, default 9:16", false),
        ],
        render: ugc_ad,
    },
    Builtin {
        name: "character_consistency",
        description: "Keep one character identical across many scenes: reference first, then every scene from it.",
        args: &[
            ("character", "Reference name of the character, or a description to create one", true),
            ("scenes", "Comma-separated scenes", false),
        ],
        render: character_consistency,
    },
    Builtin {
        name: "localization_fanout",
        description: "Turn one approved generation into a recipe and run it once per language or market.",
        args: &[
            ("job_id", "The approved job to localize", true),
            ("languages", "Comma-separated languages or markets, e.g. \"de, fr, ja\"", true),
            ("variable", "Input text to vary per language, e.g. the on-screen line", false),
        ],
        render: localization_fanout,
    },
    Builtin {
        name: "fix_provider",
        description: "Repair a provider whose API changed: read the raw exchange, edit the spec, check it, save it, retry. No rebuild or restart.",
        args: &[
            ("provider", "Provider key, e.g. \"kie\"", true),
            ("job_id", "A failed job of that provider", false),
        ],
        render: fix_provider,
    },
];

pub fn list(app: &App) -> Result<Vec<PromptInfo>> {
    let mut out: Vec<PromptInfo> = BUILTINS
        .iter()
        .map(|b| PromptInfo {
            name: b.name.into(),
            description: b.description.into(),
            arguments: b
                .args
                .iter()
                .map(|(name, description, required)| PromptArg {
                    name: (*name).into(),
                    description: (*description).into(),
                    required: *required,
                })
                .collect(),
        })
        .collect();
    for recipe in recipes::list_recipes(app)? {
        out.push(PromptInfo {
            name: format!("{RECIPE_PREFIX}{}", recipe.name),
            description: recipe_description(&recipe),
            arguments: recipe
                .variables
                .iter()
                .map(|(name, v)| PromptArg {
                    name: name.clone(),
                    description: v.description.clone().unwrap_or_default(),
                    required: v.default.is_none(),
                })
                .collect(),
        });
    }
    Ok(out)
}

pub fn get(app: &App, name: &str, args: &Map<String, Value>) -> Result<PromptText> {
    if let Some(recipe) = name.strip_prefix(RECIPE_PREFIX) {
        return recipe_prompt(app, recipe, args);
    }
    let Some(b) = BUILTINS.iter().find(|b| b.name == name) else {
        let names: Vec<&str> = BUILTINS.iter().map(|b| b.name).collect();
        bail!("no prompt `{name}`; built-in prompts: {}, plus `{RECIPE_PREFIX}<recipe name>` for every recipe", names.join(", "));
    };
    let missing: Vec<&str> = b
        .args
        .iter()
        .filter(|(n, _, required)| *required && Args(args).get(n, "").is_empty())
        .map(|(n, _, _)| *n)
        .collect();
    if !missing.is_empty() {
        let all: Vec<String> = b.args.iter().map(|(n, d, r)| format!("`{n}`{} ({d})", if *r { " required" } else { "" })).collect();
        bail!("prompt `{name}` needs {}; its arguments: {}", missing.join(", "), all.join(", "));
    }
    Ok(PromptText { name: name.into(), description: b.description.into(), text: (b.render)(&Args(args)) })
}

fn recipe_description(r: &recipes::Recipe) -> String {
    let what = if r.description.is_empty() { "Saved recipe".to_owned() } else { r.description.clone() };
    match (&r.provider, &r.model) {
        (Some(provider), Some(model)) => format!("{what} ({provider} / {model}); runs with run_recipe."),
        _ => {
            let chain: Vec<&str> = r.steps.iter().map(|s| s.name.as_str()).collect();
            format!("{what} ({} steps: {}); one run_recipe runs them all.", chain.len(), chain.join(" → "))
        }
    }
}

/// One line per variable: name, meaning, default or required.
fn variable_lines(r: &recipes::Recipe) -> String {
    if r.variables.is_empty() {
        return "Variables: none.".into();
    }
    let lines: Vec<String> = r
        .variables
        .iter()
        .map(|(name, v)| {
            let about = v.description.as_deref().map(|d| format!(": {d}")).unwrap_or_default();
            let default = v.default.as_ref().map_or_else(|| "required".to_owned(), |d| format!("default {d}"));
            format!("- `{name}`{about} ({default})")
        })
        .collect();
    format!("Variables:\n{}", lines.join("\n"))
}

fn recipe_prompt(app: &App, name: &str, args: &Map<String, Value>) -> Result<PromptText> {
    let recipe = recipes::get_recipe(app, name)?;
    // Prompt arguments arrive as strings; a value that parses as JSON (number, list) is passed as such.
    let vars: Map<String, Value> = args
        .iter()
        .filter(|(_, v)| !matches!(v, Value::String(s) if s.is_empty()))
        .map(|(k, v)| {
            let value = match v {
                Value::String(s) => serde_json::from_str::<Value>(s).ok().filter(|j| !j.is_string()).unwrap_or(v.clone()),
                other => other.clone(),
            };
            (k.clone(), value)
        })
        .collect();
    recipes::values(&recipe, &vars)?;
    let call = serde_json::json!({ "name": recipe.name, "vars": vars });
    let variables = variable_lines(&recipe);
    let text = if recipe.steps.is_empty() {
        format!(
            "Run the saved recipe `{name}`: {description}\n{variables}\n\
             1. run_recipe {call}. Add `budget` and `estimated_cost_usd` if a budget is set (list_budgets).\n\
             2. get_job {{job_id, wait_secs: 50}} until status is done; on failure get_job with debug=true.\n\
             3. Show the user assets[].path. get_recipe {{name: \"{name}\"}} shows the full template (provider, model, input).",
            description = recipe_description(&recipe),
        )
    } else {
        let steps: Vec<String> = recipe
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{}. `{}`: {}", i + 1, s.name, s.what()))
            .collect();
        format!(
            "Run the saved recipe `{name}`: {description}\nSteps, each starting once the steps it uses are done:\n{steps}\n{variables}\n\
             1. Optional: preflight {{recipe: \"{name}\", vars, budget}} checks every step and sums the estimates without sending anything.\n\
             2. run_recipe {call}. Add `budget` if one is set (list_budgets): the sum of the steps' estimates is admitted at once.\n\
             3. It returns the last step's job; get_batch {{batch_id: <its batch_id>, wait_secs: 50}} until no step is running or \
             waiting. A failed step fails the steps after it: get_job {{job_id, debug: true}} on that step shows why.\n\
             4. Show the user the batch's outputs[].path (the final results). get_recipe {{name: \"{name}\"}} shows every step's input.",
            description = recipe_description(&recipe),
            steps = steps.join("\n"),
        )
    };
    Ok(PromptText { name: format!("{RECIPE_PREFIX}{name}"), description: recipe_description(&recipe), text })
}

fn product_shoot(a: &Args) -> String {
    let product = a.get("product", "");
    let scenes = a.get("scenes", "three scenes that suit the product");
    let collection = a.get("collection", &format!("{product} shoot"));
    format!(
        "Product shoot for `{product}`, scenes: {scenes}. The product must look identical in every shot.\n\
         1. Lock the product: get_reference {{reference: \"{product}\"}}. If it does not exist, find its photos with search_assets \
         or import_asset {{path}}, then save_reference {{name: \"{product}\", kind: \"product\", description: <exact shape, colors, \
         label text, materials>, asset_ids: [clean packshots, front first]}}.\n\
         2. Optional brand style: save_snippet {{name: \"brand\", text: <lighting, palette, lens, mood>}} once, reuse it as {{{{snippet:brand}}}}.\n\
         3. Pick a model that takes reference images: search_prices, then get_provider {{provider}} for model_hint and example.\n\
         4. For each scene: generate {{provider, model, input: {{prompt: \"{{{{ref:{product}.description}}}}, <scene>, {{{{snippet:brand}}}}\", \
         image_urls: [\"{{{{ref:{product}}}}}\"]}}, save_to: {{collection: \"{collection}\", reference: \"{product}\", role: \"shot\"}}}}. \
         Text placeholders keep the description word-for-word; the ref uploads every product image.\n\
         5. get_job {{job_id, wait_secs: 50}} for each until done. Compare outputs to the packshot; regenerate any where the label, \
         shape or color drifted.\n\
         6. When one shot is right, save_recipe {{name: \"{product} shot\", from_job: <job id>}} and replace the scene text in its \
         input with {{{{var:scene}}}}; later scenes are run_recipe {{name, vars: {{scene}}}}.\n\
         7. export_collection {{collection: \"{collection}\"}} packs the shoot with its metadata."
    )
}

fn ugc_ad(a: &Args) -> String {
    let product = a.get("product", "");
    let persona = a.get("persona", "a friendly creator in their late twenties");
    let script = a.get("script", "a 2-sentence hook-first testimonial");
    let aspect = a.get("aspect", "9:16");
    format!(
        "UGC ad for `{product}`: {persona} says {script}. Aspect {aspect}, handheld phone look.\n\
         1. References: get_reference {{reference: \"{product}\"}} (create with save_reference kind \"product\" if missing). For the \
         persona, list_references {{kind: \"character\"}}; if none fits, generate a portrait first, then save_reference {{name, \
         kind: \"character\", description, asset_ids: [<portrait asset>]}}.\n\
         2. Still: pick an image model with reference input (search_prices, get_provider). generate {{input: {{prompt: \
         \"selfie-style photo, {{{{ref:<persona>.description}}}} holding {{{{ref:{product}.description}}}}, natural light, {aspect}\", \
         image_urls: [\"{{{{ref:<persona>}}}}\", \"{{{{ref:{product}}}}}\"]}}, save_to: {{collection: \"{product} ugc\"}}}}; \
         get_job {{wait_secs: 50}}. Pick the still where the product is sharp and correct.\n\
         3. Video: pick an image-to-video model with speech or lip-sync (search_prices {{mode: \"generate\"}}, get_provider). \
         generate {{input: {{prompt: \"handheld, the person says: <script line>\", image_url: \"{{{{asset:<still id>}}}}\", \
         duration: 5-10}}, save_to: {{collection: \"{product} ugc\"}}}}; get_job until done.\n\
         4. Check that the product label stayed readable; if it melted, regenerate from the still with less camera motion.\n\
         5. Variants: save_recipe {{name: \"{product} ugc\", from_job: <video job>}} with {{{{var:line}}}} in the prompt, then \
         run_recipe per hook. list_jobs {{collection: \"{product} ugc\"}} is the history."
    )
}

fn character_consistency(a: &Args) -> String {
    let character = a.get("character", "");
    let scenes = a.get("scenes", "the scenes the user asked for");
    format!(
        "Keep `{character}` identical across: {scenes}.\n\
         1. get_reference {{reference: \"{character}\"}}. If it does not exist: generate a neutral front portrait (plain background, \
         even light), get_job {{wait_secs: 50}}, then save_reference {{name: \"{character}\", kind: \"character\", description: <face, \
         hair, build, outfit, distinctive marks>, asset_ids: [<portrait>]}}. Add a side view with add_to_reference {{role: \"side\"}}.\n\
         2. Pick a model that accepts reference images (search_prices, get_provider; get_stats if present shows what worked before).\n\
         3. For each scene: generate {{input: {{prompt: \"{{{{ref:{character}.description}}}}, <scene>\", image_urls: \
         [\"{{{{ref:{character}}}}}\"]}}, save_to: {{reference: \"{character}\", role: \"generated\", collection: <project>}}}}.\n\
         4. get_job {{wait_secs: 50}} for each. Reject drifted faces: remove_from_reference {{reference: \"{character}\", asset_ids}} \
         so they are not used as references later.\n\
         5. get_lineage {{asset_id}} on any output shows which reference images it came from."
    )
}

fn localization_fanout(a: &Args) -> String {
    let job = a.get("job_id", "");
    let languages = a.get("languages", "");
    let variable = a.get("variable", "the text that changes per language");
    format!(
        "Localize job `{job}` into: {languages}.\n\
         1. get_job {{job_id: \"{job}\"}}: read provider, model and input; find {variable}.\n\
         2. save_recipe {{name: <short name>, description, provider, model, input: <the job's input with that text replaced by \
         {{{{var:text}}}} and the language by {{{{var:language}}}}>, variables: {{text: {{description: \"translated line\"}}, \
         language: {{description: \"target language\"}}}}, save_to: {{collection: <project> localized}}}}. \
         (save_recipe {{name, from_job: \"{job}\"}} copies it unchanged; edit and save it again to add the variables.)\n\
         3. Translate the line yourself for each of: {languages}.\n\
         4. run_recipe {{name, vars: {{text: <translation>, language: <language>}}}} once per language. With a budget, pass budget \
         and estimated_cost_usd on each call.\n\
         5. get_job {{wait_secs: 50}} for each job; list_jobs {{collection}} shows all versions side by side. Rerun failed ones."
    )
}

fn fix_provider(a: &Args) -> String {
    let provider = a.get("provider", "");
    let job = a.get("job_id", "<the failed job>");
    format!(
        "Fix provider `{provider}`; its API changed. No rebuild or restart is needed.\n\
         1. get_job {{job_id: \"{job}\", debug: true}}: the trace has the exact request and raw response of each step. Find the field \
         that moved (task id, status, output URL) or the new error text.\n\
         2. get_spec {{provider: \"{provider}\"}}: the active TOML and its fixture names (docs/provider-spec.md describes each field).\n\
         3. Edit the TOML: pointers, URLs, status values, body fields.\n\
         4. check_spec {{spec: <edited text>, model, input}}: validates, replays recorded fixtures and renders the exact requests. \
         Repeat until valid; a failing fixture means the recorded response must be updated too.\n\
         5. save_spec {{spec: <edited text>}}: active immediately in every process.\n\
         6. generate the same input again; get_job {{wait_secs: 50}} until done.\n\
         7. If the fix is wrong, reset_spec {{provider: \"{provider}\"}} returns to the built-in spec."
    )
}
