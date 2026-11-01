//! Prompt blocks and saved generation templates.
//!
//! Text placeholders sit anywhere inside a string of generation input and are replaced by text
//! before media placeholders are resolved: `{{snippet:<name>}}`, `{{ref:<name>.description}}`,
//! `{{ref:<name>.tags}}`. Recipes add `{{var:<name>}}`, filled by run_recipe, and a chain recipe's
//! steps add `{{step:<name>}}`, which run_recipe turns into the `{{job:…}}` of that step's job.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Row, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    app::{App, NotFound},
    db::now,
    jobs::{self, Job, SaveTo},
    library::media::Op,
};

const MAX_STEPS: usize = 20;

#[derive(Debug, Serialize, JsonSchema)]
pub struct Snippet {
    pub name: String,
    pub text: String,
    pub description: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Variable {
    /// What the value means, e.g. "language of the on-screen text".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Used when run_recipe gets no value; without one the variable is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

/// One step of a chain recipe: a generation (`provider` + `model`) or an ffmpeg edit (`op`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Step {
    /// Unique in the recipe, e.g. "still". Later steps take its outputs as "{{step:still}}" (all in an
    /// array, else the first), "{{step:still:<n>}}" (output n, from 0) or "{{step:still:last_frame}}" / "first_frame".
    pub name: String,
    /// Generation step: provider key and model, as in generate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Edit step instead, run locally with ffmpeg once the steps it uses are done; `input` holds that tool's parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<Op>,
    /// Generation input, or the edit's parameters with assets as "{{step:…}}", "{{asset:<id>}}" or plain ids.
    /// "{{var:…}}" and every placeholder of generate work here.
    #[serde(default)]
    pub input: Map<String, Value>,
    /// Where this step's outputs are filed; the recipe's save_to covers its final steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_to: Option<SaveTo>,
    /// Expected USD cost of this generation, for budgets and estimates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    /// Offer from search_prices that prices this generation from its input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer_id: Option<String>,
}

impl Step {
    /// "kie / emu" or "edit trim", for listings.
    pub fn what(&self) -> String {
        match (self.op, &self.provider, &self.model) {
            (Some(op), ..) => format!("edit {}", op.as_str()),
            (None, Some(p), Some(m)) => format!("generate {p} / {m}"),
            _ => "incomplete".into(),
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Recipe {
    pub name: String,
    pub description: String,
    /// Single-generation recipe: provider, model and input. Absent when the recipe has `steps`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Generation input with `{{var:<name>}}` (and any other placeholders) left in place.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    /// A chain started by one run_recipe, in order; run_recipe returns the last step's job.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    /// Where the outputs are filed; with steps, the outputs of the final steps (those no later step uses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save_to: Option<SaveTo>,
    /// Offer from search_prices that prices every run; single-generation recipes only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offer_id: Option<String>,
    /// Budget every run counts against unless the call names another.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<String>,
    pub variables: BTreeMap<String, Variable>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct RecipeFields {
    pub name: String,
    pub description: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub input: Option<Value>,
    pub steps: Vec<Step>,
    pub save_to: Option<SaveTo>,
    pub offer_id: Option<String>,
    pub budget: Option<String>,
    pub variables: BTreeMap<String, Variable>,
}

pub fn save_snippet(app: &App, name: &str, text: &str, description: Option<&str>) -> Result<Snippet> {
    check_name(name, "snippet")?;
    ensure!(!text.contains("{{snippet:"), "a snippet cannot include another snippet; inline its text instead");
    let conn = app.db.conn();
    conn.execute(
        "INSERT INTO snippets (name, text, description, created_at, updated_at) VALUES (?1, ?2, COALESCE(?3, ''), ?4, ?4)
         ON CONFLICT (name) DO UPDATE SET text = excluded.text,
             description = COALESCE(?3, description), updated_at = excluded.updated_at",
        params![name, text, description, now()],
    )?;
    find_snippet(&conn, name)?.ok_or_else(|| anyhow!("snippet `{name}` vanished"))
}

pub fn list_snippets(app: &App) -> Result<Vec<Snippet>> {
    let conn = app.db.conn();
    let mut stmt = conn.prepare(&format!("{SNIPPET_SELECT} ORDER BY name"))?;
    let rows = stmt.query_map([], snippet_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn delete_snippet(app: &App, name: &str) -> Result<Snippet> {
    let conn = app.db.conn();
    let snippet = find_snippet(&conn, name)?
        .ok_or_else(|| NotFound(format!("no snippet `{name}`; {}", known(&conn, "snippets").unwrap_or_default())))?;
    conn.execute("DELETE FROM snippets WHERE name = ?1", [name])?;
    Ok(snippet)
}

const SNIPPET_SELECT: &str = "SELECT name, text, description, updated_at FROM snippets";

fn find_snippet(conn: &Connection, name: &str) -> Result<Option<Snippet>> {
    let mut stmt = conn.prepare_cached(&format!("{SNIPPET_SELECT} WHERE name = ?1"))?;
    Ok(stmt.query_row([name], snippet_row).optional()?)
}

fn snippet_row(r: &Row) -> rusqlite::Result<Snippet> {
    Ok(Snippet { name: r.get(0)?, text: r.get(1)?, description: r.get(2)?, updated_at: r.get(3)? })
}

/// Variables used in `input` or a step but not declared are added without a default (so they are required).
pub fn save_recipe(app: &App, mut f: RecipeFields) -> Result<Recipe> {
    check_name(&f.name, "recipe")?;
    let mut used = BTreeSet::new();
    if f.steps.is_empty() {
        let (Some(provider), Some(_), Some(input)) = (&f.provider, &f.model, &f.input) else {
            bail!("recipe `{}` needs provider, model and input, or steps", f.name)
        };
        app.find_provider(provider)?;
        ensure!(input.is_object(), "recipe input must be a JSON object");
        collect_vars(input, &mut used);
    } else {
        ensure!(
            f.provider.is_none() && f.model.is_none() && f.input.is_none() && f.offer_id.is_none() && f.budget.is_none(),
            "recipe `{}` has steps: put provider, model, input and offer_id into a step, and pass budget to run_recipe",
            f.name
        );
        check_steps(app, &f.steps)?;
        for step in &f.steps {
            step.input.values().for_each(|v| collect_vars(v, &mut used));
        }
    }
    for name in used {
        f.variables.entry(name).or_default();
    }
    let conn = app.db.conn();
    conn.execute(
        "INSERT INTO recipes (name, description, provider, model, input, steps, save_to, offer_id, budget, variables, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
         ON CONFLICT (name) DO UPDATE SET description = excluded.description, provider = excluded.provider,
             model = excluded.model, input = excluded.input, steps = excluded.steps, save_to = excluded.save_to,
             offer_id = excluded.offer_id, budget = excluded.budget,
             variables = excluded.variables, updated_at = excluded.updated_at",
        params![
            f.name,
            f.description,
            f.provider,
            f.model,
            f.input.as_ref().map(Value::to_string),
            serde_json::to_string(&f.steps)?,
            f.save_to.as_ref().map(serde_json::to_string).transpose()?,
            f.offer_id,
            f.budget,
            serde_json::to_string(&f.variables)?,
            now()
        ],
    )?;
    load_recipe(&conn, &f.name)
}

pub fn get_recipe(app: &App, name: &str) -> Result<Recipe> {
    load_recipe(&app.db.conn(), name)
}

pub fn list_recipes(app: &App) -> Result<Vec<Recipe>> {
    let conn = app.db.conn();
    let mut stmt = conn.prepare(&format!("{RECIPE_SELECT} ORDER BY name"))?;
    let rows = stmt.query_map([], recipe_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn delete_recipe(app: &App, name: &str) -> Result<Recipe> {
    let conn = app.db.conn();
    let recipe = load_recipe(&conn, name)?;
    conn.execute("DELETE FROM recipes WHERE name = ?1", [name])?;
    Ok(recipe)
}

const RECIPE_SELECT: &str =
    "SELECT name, description, provider, model, input, save_to, offer_id, budget, variables, created_at, updated_at, steps FROM recipes";

fn load_recipe(conn: &Connection, name: &str) -> Result<Recipe> {
    match conn.query_row(&format!("{RECIPE_SELECT} WHERE name = ?1"), [name], recipe_row).optional()? {
        Some(recipe) => Ok(recipe),
        None => Err(NotFound(format!("no recipe `{name}`; {}. save_recipe creates one", known(conn, "recipes")?)).into()),
    }
}

fn recipe_row(r: &Row) -> rusqlite::Result<Recipe> {
    Ok(Recipe {
        name: r.get(0)?,
        description: r.get(1)?,
        provider: r.get(2)?,
        model: r.get(3)?,
        input: json(r, 4)?,
        save_to: json(r, 5)?,
        offer_id: r.get(6)?,
        budget: r.get(7)?,
        variables: json(r, 8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
        steps: json(r, 11)?,
    })
}

/// Step names unique and usable, each step a generation or an edit, `{{step:…}}` only naming earlier steps.
fn check_steps(app: &App, steps: &[Step]) -> Result<()> {
    ensure!(steps.len() <= MAX_STEPS, "a recipe has at most {MAX_STEPS} steps, this one {}", steps.len());
    let mut earlier: Vec<&str> = Vec::new();
    for s in steps {
        let at = format!("step `{}`", s.name);
        check_name(&s.name, "step")?;
        ensure!(!s.name.contains(':'), "step name `{}` cannot contain `:`", s.name);
        ensure!(!earlier.contains(&s.name.as_str()), "two steps are named `{}`; step names must be unique", s.name);
        match (s.op, &s.provider, &s.model) {
            (Some(_), None, None) => ensure!(
                s.estimated_cost_usd.is_none() && s.offer_id.is_none(),
                "{at}: an edit runs locally at no cost; drop estimated_cost_usd and offer_id"
            ),
            (Some(_), ..) => bail!("{at}: give op (an edit) or provider and model (a generation), not both"),
            (None, Some(p), Some(_)) if p == "local" => {
                bail!("{at}: provider `local` is not a generation; write the edit as {{\"op\": \"<operation>\", \"input\": {{…}}}}")
            }
            (None, Some(p), Some(_)) => {
                app.find_provider(p).with_context(|| at.clone())?;
            }
            _ => bail!(
                "{at}: a generation step needs provider and model; an edit step needs op (extract_frame, reframe, trim, \
                 concat, overlay or mux_audio)"
            ),
        }
        for (text, name, pick) in step_refs(&s.input)? {
            if !earlier.contains(&name.as_str()) {
                let so_far = if earlier.is_empty() { "it is the first step".to_owned() } else { format!("earlier steps: {}", earlier.join(", ")) };
                bail!("{at}: `{text}` names no earlier step; {so_far}");
            }
            if let Some(pick) = pick {
                ensure!(
                    pick.parse::<usize>().is_ok() || matches!(pick.as_str(), "last_frame" | "first_frame"),
                    "{at}: `{text}`: pick an output by number from 0, or last_frame / first_frame"
                );
            }
        }
        earlier.push(&s.name);
    }
    Ok(())
}

/// `{{step:<name>}}` / `{{step:<name>:<pick>}}` as a whole string: (name, pick).
fn step_ref(s: &str) -> Result<Option<(&str, Option<&str>)>> {
    let inner = s.trim().strip_prefix("{{step:").and_then(|t| t.strip_suffix("}}")).filter(|t| !t.contains("}}"));
    match inner {
        Some(t) => Ok(Some(match t.split_once(':') {
            Some((name, pick)) => (name.trim(), Some(pick.trim())),
            None => (t.trim(), None),
        })),
        None if s.contains("{{step:") => {
            bail!("`{s}`: a step reference must be the whole string value, e.g. \"image_url\": \"{{{{step:still}}}}\"")
        }
        None => Ok(None),
    }
}

/// Every step reference in `input`: (text, name, pick).
fn step_refs(input: &Map<String, Value>) -> Result<Vec<(String, String, Option<String>)>> {
    let mut out = Vec::new();
    map_strings(&Value::Object(input.clone()), &mut |s| {
        if let Some((name, pick)) = step_ref(s)? {
            out.push((s.to_owned(), name.to_owned(), pick.map(str::to_owned)));
        }
        Ok(None)
    })?;
    Ok(out)
}

/// Steps whose outputs no other step uses: the results of a run.
pub fn finals(steps: &[Step]) -> Result<Vec<String>> {
    let mut used = HashSet::new();
    for s in steps {
        used.extend(step_refs(&s.input)?.into_iter().map(|(_, name, _)| name));
    }
    Ok(steps.iter().map(|s| s.name.clone()).filter(|n| !used.contains(n)).collect())
}

/// The step's input with every `{{step:…}}` replaced by the `{{job:…}}` of that step's job.
pub fn link(input: &Map<String, Value>, jobs: &HashMap<String, String>) -> Result<Value> {
    map_strings(&Value::Object(input.clone()), &mut |s| {
        let Some((name, pick)) = step_ref(s)? else { return Ok(None) };
        let id = jobs.get(name).ok_or_else(|| anyhow!("`{s}` names no earlier step"))?;
        Ok(Some(Value::String(match pick {
            Some(pick) => format!("{{{{job:{id}:{pick}}}}}"),
            None => format!("{{{{job:{id}}}}}"),
        })))
    })
}

/// For preflight: the input with step references shown as text, and those texts.
pub fn show_steps(input: &Map<String, Value>) -> Result<(Map<String, Value>, Vec<String>)> {
    let mut shown = Vec::new();
    let input = map_strings(&Value::Object(input.clone()), &mut |s| {
        let Some((name, pick)) = step_ref(s)? else { return Ok(None) };
        let text = match pick {
            Some(pick) => format!("output {pick} of step {name}"),
            None => format!("output of step {name}"),
        };
        shown.push(text.clone());
        Ok(Some(Value::String(format!("<{text}>"))))
    })?;
    let Value::Object(input) = input else { bail!("step input is not an object") };
    Ok((input, shown))
}

/// Rebuilds `v` with every string for which `f` returns a replacement replaced.
fn map_strings(v: &Value, f: &mut impl FnMut(&str) -> Result<Option<Value>>) -> Result<Value> {
    Ok(match v {
        Value::String(s) => f(s)?.unwrap_or_else(|| v.clone()),
        Value::Array(items) => Value::Array(items.iter().map(|i| map_strings(i, f)).collect::<Result<_>>()?),
        Value::Object(map) => Value::Object(map.iter().map(|(k, i)| Ok((k.clone(), map_strings(i, f)?))).collect::<Result<_>>()?),
        other => other.clone(),
    })
}

/// `job_id` as a step and, with `follow`, the jobs that made it, parents first: every job named by
/// `{{job:…}}` in an input, and every job that made an asset an input names (`{{asset:<id>}}`, or the
/// plain id in an edit).
pub fn chain(app: &App, job_id: &str, follow: bool) -> Result<Vec<Step>> {
    let mut order = Vec::new();
    visit(app, job_id, follow, &mut order, &mut HashSet::new())?;
    let mut names: HashMap<String, String> = HashMap::new();
    for job in &order {
        let kind = job.assets.first().and_then(|a| a.mime.as_deref()).and_then(|m| m.split('/').next());
        let base = match kind {
            _ if job.provider == "local" => job.model.clone(),
            Some(k @ ("image" | "video" | "audio")) => k.to_owned(),
            _ => "step".to_owned(),
        };
        let (mut name, mut n) = (base.clone(), 1);
        while names.values().any(|v| *v == name) {
            n += 1;
            name = format!("{base}-{n}");
        }
        names.insert(job.id.clone(), name);
    }
    let conn = app.db.conn();
    order
        .iter()
        .map(|job| {
            let input = map_strings(&job.input, &mut |s| {
                Ok(parent_of(&conn, job, s)?.and_then(|(parent, pick)| {
                    let name = names.get(&parent)?;
                    Some(Value::String(match pick {
                        Some(pick) => format!("{{{{step:{name}:{pick}}}}}"),
                        None => format!("{{{{step:{name}}}}}"),
                    }))
                }))
            })?;
            let Value::Object(input) = input else { bail!("job {} has an input that is not an object", job.id) };
            let op = (job.provider == "local")
                .then(|| serde_json::from_value::<Op>(Value::String(job.model.clone())))
                .transpose()
                .with_context(|| format!("job {} ran `{}`, which a recipe step cannot repeat", job.id, job.model))?;
            Ok(Step {
                name: names[&job.id].clone(),
                provider: op.is_none().then(|| job.provider.clone()),
                model: op.is_none().then(|| job.model.clone()),
                op,
                input,
                save_to: job.save_to.clone(),
                ..Default::default()
            })
        })
        .collect()
}

fn visit(app: &App, id: &str, follow: bool, order: &mut Vec<Job>, seen: &mut HashSet<String>) -> Result<()> {
    if !seen.insert(id.to_owned()) {
        return Ok(());
    }
    ensure!(seen.len() <= MAX_STEPS, "the chain has more than {MAX_STEPS} jobs; save_recipe with explicit steps instead");
    let job = jobs::get(app, id, false)?;
    let mut parents = Vec::new();
    if follow {
        let conn = app.db.conn();
        map_strings(&job.input, &mut |s| {
            parents.extend(parent_of(&conn, &job, s)?.map(|(p, _)| p));
            Ok(None)
        })?;
    }
    for parent in parents {
        visit(app, &parent, follow, order, seen).with_context(|| format!("walking the inputs of job {id}"))?;
    }
    order.push(job);
    Ok(())
}

/// The job behind one string of `job`'s input, and which of its outputs the string picks.
fn parent_of(conn: &Connection, job: &Job, s: &str) -> Result<Option<(String, Option<String>)>> {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("{{job:").and_then(|r| r.strip_suffix("}}")) {
        return Ok(Some(match rest.split_once(':') {
            Some((id, pick)) => (id.trim().to_owned(), Some(pick.trim().to_owned())),
            None => (rest.trim().to_owned(), None),
        }));
    }
    let asset = match t.strip_prefix("{{asset:").and_then(|r| r.strip_suffix("}}")) {
        Some(id) => id.trim(),
        None if job.provider == "local" && job.inputs.iter().any(|i| i.asset_id == t) => t,
        None => return Ok(None),
    };
    let made_by: Option<String> =
        conn.query_row("SELECT job_id FROM assets WHERE id = ?1", [asset], |r| r.get(0)).optional()?.flatten();
    let Some(parent) = made_by else { return Ok(None) };
    let mut stmt = conn.prepare_cached(
        "SELECT o.asset_id FROM job_outputs o JOIN assets a ON a.id = o.asset_id WHERE o.job_id = ?1 ORDER BY o.rowid",
    )?;
    let outputs: Vec<String> = stmt.query_map([&parent], |r| r.get(0))?.collect::<Result<_, _>>()?;
    let pick = if outputs.len() > 1 { outputs.iter().position(|o| o == asset).map(|i| i.to_string()) } else { None };
    Ok(Some((parent, pick)))
}

/// A JSON text column; NULL reads as JSON null.
fn json<T: serde::de::DeserializeOwned>(r: &Row, i: usize) -> rusqlite::Result<T> {
    let text: Option<String> = r.get(i)?;
    serde_json::from_str(text.as_deref().unwrap_or("null"))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e)))
}

/// The single-generation recipe's input with every `{{var:…}}` filled from `vars`, then from defaults.
pub fn fill(recipe: &Recipe, vars: &Map<String, Value>) -> Result<Value> {
    fill_value(single(recipe)?, &values(recipe, vars)?)
}

/// The input of a single-generation recipe; an error for a recipe with steps.
pub fn single(recipe: &Recipe) -> Result<&Value> {
    recipe.input.as_ref().ok_or_else(|| {
        anyhow!(
            "recipe `{}` has {} steps: run_recipe runs the chain; generate_batch takes single-generation recipes only",
            recipe.name,
            recipe.steps.len()
        )
    })
}

/// The chain recipe's steps with every `{{var:…}}` filled.
pub fn fill_steps(recipe: &Recipe, vars: &Map<String, Value>) -> Result<Vec<Step>> {
    let values = values(recipe, vars)?;
    recipe
        .steps
        .iter()
        .map(|s| {
            let input = s.input.iter().map(|(k, v)| Ok((k.clone(), fill_value(v, &values)?))).collect::<Result<_>>()?;
            Ok(Step { input, ..s.clone() })
        })
        .collect()
}

/// Every variable's value from `vars`, then from defaults; an error names what is missing or unknown.
pub fn values(recipe: &Recipe, vars: &Map<String, Value>) -> Result<Map<String, Value>> {
    let listing = || {
        recipe
            .variables
            .iter()
            .map(|(name, v)| {
                let mut line = format!("`{name}`");
                if let Some(d) = &v.description {
                    line += &format!(" ({d})");
                }
                match &v.default {
                    Some(default) => line += &format!(" default {default}"),
                    None => line += " required",
                }
                line
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(unknown) = vars.keys().find(|k| !recipe.variables.contains_key(*k)) {
        bail!("recipe `{}` has no variable `{unknown}`; its variables: {}", recipe.name, listing());
    }
    let mut values = Map::new();
    let mut missing = Vec::new();
    for (name, v) in &recipe.variables {
        match vars.get(name).or(v.default.as_ref()) {
            Some(value) => {
                values.insert(name.clone(), value.clone());
            }
            None => missing.push(format!("`{name}`")),
        }
    }
    if !missing.is_empty() {
        bail!(
            "recipe `{}` needs {} in `vars`; its variables: {}",
            recipe.name,
            missing.join(", "),
            listing()
        );
    }
    Ok(values)
}

fn fill_value(v: &Value, vars: &Map<String, Value>) -> Result<Value> {
    Ok(match v {
        Value::String(s) => {
            let whole = s.trim().strip_prefix("{{var:").and_then(|t| t.strip_suffix("}}"));
            match whole.filter(|name| !name.contains("}}")) {
                Some(name) => var(vars, name.trim())?.clone(),
                None => Value::String(replace_tokens(s, "var:", |name| {
                    Ok(Some(match var(vars, name)? {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    }))
                })?),
            }
        }
        Value::Array(items) => Value::Array(items.iter().map(|i| fill_value(i, vars)).collect::<Result<_>>()?),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, i)| Ok((k.clone(), fill_value(i, vars)?))).collect::<Result<_>>()?)
        }
        other => other.clone(),
    })
}

fn var<'a>(vars: &'a Map<String, Value>, name: &str) -> Result<&'a Value> {
    vars.get(name).ok_or_else(|| anyhow!("`{{{{var:{name}}}}}` is not a declared variable; save_recipe again to declare it"))
}

fn collect_vars(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::String(s) => {
            let _ = replace_tokens(s, "var:", |name| {
                out.insert(name.to_owned());
                Ok(None)
            });
        }
        Value::Array(items) => items.iter().for_each(|i| collect_vars(i, out)),
        Value::Object(map) => map.values().for_each(|i| collect_vars(i, out)),
        _ => {}
    }
}

/// Replaces text placeholders inside every string of `input`. `{{ref:<name>}}` without a field
/// stays: it is a media placeholder, resolved later by uploading the reference's assets.
pub fn expand_text(app: &App, input: &Value) -> Result<Value> {
    if !input.to_string().contains("{{") {
        return Ok(input.clone());
    }
    expand_value(&app.db.conn(), input)
}

fn expand_value(conn: &Connection, v: &Value) -> Result<Value> {
    Ok(match v {
        Value::String(s) if s.contains("{{") => {
            let s = replace_tokens(s, "snippet:", |name| {
                match find_snippet(conn, name)? {
                    Some(snippet) => Ok(Some(snippet.text)),
                    None => bail!("no snippet `{name}`; {}. save_snippet creates one", known(conn, "snippets")?),
                }
            })?;
            Value::String(replace_tokens(&s, "ref:", |name| ref_text(conn, name))?)
        }
        Value::Array(items) => Value::Array(items.iter().map(|i| expand_value(conn, i)).collect::<Result<_>>()?),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, i)| Ok((k.clone(), expand_value(conn, i)?))).collect::<Result<_>>()?)
        }
        other => other.clone(),
    })
}

fn ref_text(conn: &Connection, placeholder: &str) -> Result<Option<String>> {
    let (name, field) = match placeholder.rsplit_once('.') {
        Some((name, field @ ("description" | "tags"))) => (name.trim(), field),
        _ => return Ok(None),
    };
    let sql = format!("SELECT {field} FROM refs WHERE id = ?1 OR name = ?1");
    let text: Option<String> = conn.query_row(&sql, [name], |r| r.get(0)).optional()?;
    match text {
        Some(text) => Ok(Some(text)),
        None => bail!("no reference `{name}` (in `{{{{ref:{placeholder}}}}}`); {}. save_reference creates one", known(conn, "refs")?),
    }
}

/// Calls `f` with the trimmed name of every `{{<prefix><name>}}` in `s`; `Some` replaces the token.
fn replace_tokens(s: &str, prefix: &str, mut f: impl FnMut(&str) -> Result<Option<String>>) -> Result<String> {
    let open = format!("{{{{{prefix}");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find("}}") else { break };
        out.push_str(&rest[..start]);
        let token = &rest[start..start + open.len() + end + 2];
        match f(after[..end].trim())? {
            Some(text) => out.push_str(&text),
            None => out.push_str(token),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// "known snippets: a, b" for error messages.
fn known(conn: &Connection, table: &str) -> Result<String> {
    let label = if table == "refs" { "references" } else { table };
    let mut stmt = conn.prepare(&format!("SELECT name FROM {table} ORDER BY name"))?;
    let names: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    Ok(if names.is_empty() { format!("no {label} exist yet") } else { format!("known {label}: {}", names.join(", ")) })
}

fn check_name(name: &str, what: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty() && !name.contains("{{") && !name.contains("}}") && name.trim() == name,
        "`{name}` is not a usable {what} name: use letters, digits, spaces, `-` or `_`"
    );
    Ok(())
}
