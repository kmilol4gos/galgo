//! Lectura local, incremental y sin red de métricas de uso. Nunca se guardan mensajes.
//! El daemon debe llamar a `poll` fuera del hilo async. Los `.zst` requieren `zstd` en PATH.
mod parser;
mod prices;
#[cfg(test)]
mod tests;
pub use prices::PRICES_TOML;
use parser::*;
use serde::{Deserialize, Serialize};
use std::{collections::{HashMap, HashSet}, fs::File, hash::{Hash, Hasher}, collections::hash_map::DefaultHasher, io::{self, BufRead, BufReader, Read, Seek, SeekFrom}, os::unix::fs::MetadataExt, path::{Path, PathBuf}, process::{Command, Stdio}, time::{Instant, SystemTime, UNIX_EPOCH}};

const INITIAL_TAIL: u64 = 100 * 1024 * 1024;
const MAX_LINE: usize = 8 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all="lowercase")]
pub enum Agent { Claude, Codex, Pi, Opencode }
impl Agent { fn name(self) -> &'static str { match self { Self::Claude => "claude", Self::Codex => "codex", Self::Pi => "pi", Self::Opencode => "opencode" } } }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum CostSource { Agent, Statusline, ListPrice, Unknown }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneUsage {
    pub agent: String, pub model: Option<String>, pub input: u64, pub cache_read: u64,
    pub cache_write: u64, pub cache_write_5m: u64, pub cache_write_1h: u64,
    pub output: u64, pub reasoning: u64, pub cost_usd: Option<f64>, pub cost_source: CostSource,
    pub context_tokens: Option<u64>, pub context_window: Option<u64>, pub context_pct: Option<f32>,
    pub updated_secs: u64,
    /// Los totales no incluyen todo el archivo (cola inicial o línea demasiado grande/inválida).
    pub partial: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LimitWindow { pub minutes: u64, pub used_pct: f32, pub resets_at: Option<u64> }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limits { pub agent: String, pub plan: Option<String>, pub windows: Vec<LimitWindow>, pub observed_at: u64 }
#[derive(Clone)]
struct Accum {
    usage: PaneUsage, model: Model, seen: HashSet<Id>, last_codex: Option<CodexUsage>, cost_valid: bool,
    limits: Option<Limits>, updated: Option<Instant>,
}
impl Accum {
    fn new(agent: Agent) -> Self { Self { usage: PaneUsage { agent: agent.name().into(), model: None, input: 0, cache_read: 0, cache_write: 0, cache_write_5m: 0, cache_write_1h: 0, output: 0, reasoning: 0, cost_usd: None, cost_source: CostSource::Unknown, context_tokens: None, context_window: None, context_pct: None, updated_secs: 0, partial: false }, model: Model::Unknown, seen: HashSet::new(), last_codex: None, cost_valid: true, limits: None, updated: None } }
    fn partial(&mut self) { self.usage.partial = true; self.cost_valid = false; self.usage.cost_usd = None; self.usage.cost_source = CostSource::Unknown; }
    /// Un modelo desconocido no borra el último conocido: un mensaje raro no deja el panel sin modelo.
    fn model(&mut self, model: Option<Model>) { if let Some(m) = model.filter(|m| *m != Model::Unknown) { self.model = m; self.usage.model = m.name().map(str::to_owned); } }
    fn cost(&mut self, cost: Option<f64>, source: CostSource) {
        if cost.is_none_or(|v| !v.is_finite() || v < 0.0) { self.cost_valid = false; }
        if self.cost_valid { self.usage.cost_usd = Some(self.usage.cost_usd.unwrap_or(0.0) + cost.unwrap()); self.usage.cost_source = source; }
        else { self.usage.cost_usd = None; self.usage.cost_source = CostSource::Unknown; }
    }
    fn line(&mut self, agent: Agent, line: &[u8]) {
        match agent {
            Agent::Claude if contains(line,b"assistant") && contains(line,b"usage") => {
                let Ok(event) = serde_json::from_slice::<ClaudeLine>(line) else { self.partial(); return };
                if event.kind != Kind::Assistant { return; }
                let Some(msg) = event.message else { return }; let Some(u) = msg.usage else { return };
                if event.sidechain == Some(false) {
                    if let Some(id) = &msg.model { self.usage.model = Some(id.0.clone()); }
                    self.usage.context_tokens = Some(u.input_tokens.saturating_add(u.cache_read_input_tokens).saturating_add(u.cache_creation_input_tokens));
                    self.usage.context_window = None; self.usage.context_pct = None;
                }
                let Some(id) = msg.id.or(event.request_id) else { self.partial(); return };
                if !self.seen.insert(id) { return; }
                self.usage.input = self.usage.input.saturating_add(u.input_tokens);
                self.usage.output = self.usage.output.saturating_add(u.output_tokens);
                self.usage.reasoning = self.usage.reasoning.saturating_add(u.output_tokens_details.map_or(0, |d| d.thinking_tokens));
                self.usage.cache_read = self.usage.cache_read.saturating_add(u.cache_read_input_tokens);
                self.usage.cache_write = self.usage.cache_write.saturating_add(u.cache_creation_input_tokens);
                let c = u.cache_creation.unwrap_or_default();
                self.usage.cache_write_5m = self.usage.cache_write_5m.saturating_add(c.ephemeral_5m_input_tokens);
                self.usage.cache_write_1h = self.usage.cache_write_1h.saturating_add(c.ephemeral_1h_input_tokens);
                let price = msg.model.as_ref().and_then(|m| prices::price_id(&m.0)).and_then(|p| {
                    if u.speed.is_some_and(|s| s != Speed::Standard) || u.inference_geo.is_some_and(|g| g != Geo::Global)
                        || u.server_tool_use.is_some_and(|t| t.web_search_requests > 0 || t.web_fetch_requests > 0) { return None; }
                    if c.ephemeral_5m_input_tokens.saturating_add(c.ephemeral_1h_input_tokens) != u.cache_creation_input_tokens { return None; }
                    if (c.ephemeral_5m_input_tokens > 0 && p.write5.is_none()) || (c.ephemeral_1h_input_tokens > 0 && p.write1h.is_none()) { return None; }
                    Some((u.input_tokens as f64*p.input + u.cache_read_input_tokens as f64*p.read + u.output_tokens as f64*p.output + c.ephemeral_5m_input_tokens as f64*p.write5.unwrap_or(0.0) + c.ephemeral_1h_input_tokens as f64*p.write1h.unwrap_or(0.0))/1_000_000.0)
                });
                self.cost(price, CostSource::ListPrice); self.updated = Some(Instant::now());
            }
            Agent::Codex if contains(line,b"token_count") || contains(line,b"turn_context") => {
                let Ok(event) = serde_json::from_slice::<CodexLine>(line) else { self.partial(); return };
                let Some(p) = event.payload else { return };
                if event.kind == Kind::TurnContext { self.model(p.model); return; }
                if event.kind != Kind::EventMsg || p.kind != Some(Kind::TokenCount) { return; }
                if let Some(info) = p.info {
                    if let Some(last) = info.last_token_usage {
                        self.usage.context_tokens = Some(last.input_tokens.saturating_add(last.output_tokens));
                        self.usage.context_window = info.model_context_window.filter(|x| *x > 0);
                        self.usage.context_pct = self.usage.context_window.map(|w| self.usage.context_tokens.unwrap() as f32 * 100.0 / w as f32);
                    }
                    if let Some(total) = info.total_token_usage { self.codex_total(total, info.last_token_usage); }
                }
                if let Some(r) = p.rate_limits {
                    let windows = [r.primary,r.secondary].into_iter().flatten().filter_map(|w| {
                        let minutes = w.window_minutes?; let used_pct = w.used_percent?;
                        if !used_pct.is_finite() || !(0.0..=100.0).contains(&used_pct) { return None; }
                        Some(LimitWindow { minutes, used_pct, resets_at: w.resets_at })
                    }).collect();
                    let plan = r.plan_type.and_then(|p| Some(match p { Plan::Plus => "plus", Plan::Pro => "pro", Plan::Team => "team", Plan::Enterprise => "enterprise", Plan::Free => "free", Plan::Other => return None })).map(str::to_owned);
                    self.limits = Some(Limits { agent: "codex".into(), plan, windows, observed_at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() });
                }
                self.updated = Some(Instant::now());
            }
            Agent::Pi if contains(line,b"assistant") && contains(line,b"usage") => {
                let Ok(event) = serde_json::from_slice::<PiLine>(line) else { self.partial(); return };
                if event.kind != Kind::Message { return; } let Some(msg) = event.message else { return };
                if msg.role != Role::Assistant { return; } let Some(u) = msg.usage else { return };
                self.model(msg.model);
                self.usage.input = self.usage.input.saturating_add(u.input);
                self.usage.output = self.usage.output.saturating_add(u.output);
                self.usage.reasoning = self.usage.reasoning.saturating_add(u.reasoning);
                self.usage.cache_read = self.usage.cache_read.saturating_add(u.cache_read);
                self.usage.cache_write = self.usage.cache_write.saturating_add(u.cache_write);
                self.usage.context_tokens = Some(u.input.saturating_add(u.cache_read).saturating_add(u.cache_write));
                let cost = u.cost.and_then(|c| { let _provider = c.total_from_provider; c.total });
                self.cost(cost, CostSource::Agent); self.updated = Some(Instant::now());
            }
            _ => (),
        }
    }
    fn codex_total(&mut self, total: CodexUsage, last: Option<CodexUsage>) {
        let old = self.last_codex.unwrap_or_default();
        let delta = total.input_tokens.checked_sub(old.input_tokens).zip(total.cached_input_tokens.checked_sub(old.cached_input_tokens)).zip(total.output_tokens.checked_sub(old.output_tokens)).zip(total.cache_write_input_tokens.checked_sub(old.cache_write_input_tokens));
        self.usage.input = total.input_tokens.saturating_sub(total.cached_input_tokens);
        self.usage.cache_read = total.cached_input_tokens; self.usage.cache_write = total.cache_write_input_tokens;
        self.usage.output = total.output_tokens; self.usage.reasoning = total.reasoning_output_tokens;
        let cost = delta.and_then(|(((input, cached), output), write)| {
            let uncached = input.checked_sub(cached)?;
            if write != 0 || total.cached_input_tokens > total.input_tokens || last.is_none_or(|u| u.input_tokens > 272_000) { return None; }
            let p = prices::price(self.model)?;
            Some((uncached as f64*p.input + cached as f64*p.read + output as f64*p.output)/1_000_000.0)
        });
        self.last_codex = Some(total); self.cost(cost, CostSource::ListPrice);
    }
}
#[derive(Clone, Copy)]
struct Cursor { dev: u64, inode: u64, size: u64, offset: u64, mtime: (i64,i64) }
struct Tracked { agent: Agent, session_hash: u64, path: PathBuf, cursor: Option<Cursor>, accum: Accum, error: Option<String> }
#[derive(Default)]
pub struct UsageTracker { panes: HashMap<String, Tracked> }
impl UsageTracker {
    pub fn new() -> Self { Self::default() }
    /// Registrar de nuevo la misma fuente conserva los offsets; cambiar sesión o ruta reinicia.
    pub fn track(&mut self, pane_key: &str, agent: Agent, session_id: &str, transcript_path: impl AsRef<Path>) {
        let mut h = DefaultHasher::new(); session_id.hash(&mut h); let session_hash = h.finish();
        let path = transcript_path.as_ref();
        if self.panes.get(pane_key).is_some_and(|t| t.agent == agent && t.session_hash == session_hash && t.path == path) { return; }
        self.panes.insert(pane_key.to_owned(), Tracked { agent, session_hash, path: path.to_owned(), cursor: None, accum: Accum::new(agent), error: None });
    }
    /// Actualiza desde el último offset. No devuelve datos hasta observar una métrica válida.
    /// Los fallos se consultan con `last_error`; no incluyen rutas ni fragmentos del transcript.
    pub fn poll(&mut self, pane_key: &str) -> Option<PaneUsage> {
        let t = self.panes.get_mut(pane_key)?;
        match poll_file(t) { Ok(()) => t.error = None, Err(message) => t.error = Some(message.to_owned()) }
        let updated = t.accum.updated?; let mut result = t.accum.usage.clone(); result.updated_secs = updated.elapsed().as_secs(); Some(result)
    }
    pub fn last_error(&self, pane_key: &str) -> Option<&str> { self.panes.get(pane_key)?.error.as_deref() }
    pub fn limits(&self, pane_key: &str) -> Option<&Limits> { self.panes.get(pane_key)?.accum.limits.as_ref() }
    pub fn untrack(&mut self, pane_key: &str) { self.panes.remove(pane_key); }
}
fn poll_file(t: &mut Tracked) -> Result<(), &'static str> {
    let mut file = File::open(&t.path).map_err(|_| "no se pudo abrir la fuente de uso")?;
    let meta = file.metadata().map_err(|_| "no se pudo leer la metadata de uso")?;
    if !meta.is_file() { return Err("la fuente de uso no es un archivo regular"); }
    let size = meta.len(); let mtime = (meta.mtime(),meta.mtime_nsec());
    let replace = t.cursor.is_some_and(|c| c.dev != meta.dev() || c.inode != meta.ino() || size < c.size);
    let compressed = t.agent == Agent::Codex && t.path.extension().is_some_and(|s| s == "zst");
    let mut acc = if replace { Accum::new(t.agent) } else { t.accum.clone() };
    let mut offset = if replace { 0 } else { t.cursor.map_or(0, |c| c.offset) };
    if !replace && t.cursor.is_some_and(|c| c.size == size && c.mtime == mtime) { return Ok(()); }
    let mut discard_first = false;
    if !compressed && t.cursor.is_none() && size > INITIAL_TAIL {
        offset = size - INITIAL_TAIL; discard_first = true; acc.partial();
    }
    if compressed {
        // El seek sobre zstd no es posible. Para un archivo inicial grande se mide el flujo sin
        // guardarlo y solo se parsea la cola; la descompresión ha de recorrerlo igualmente.
        if t.cursor.is_none() && size > INITIAL_TAIL {
            let copy = file.try_clone().map_err(|_| "no se pudo duplicar la fuente de uso")?;
            let mut child = start_zstd("zstd", copy)?;
            let mut out = child.stdout.take().ok_or("no se pudo leer zstd")?;
            let count = io::copy(&mut out, &mut io::sink());
            let status = child.wait().map_err(|_| "no se pudo esperar a zstd")?;
            if !status.success() { return Err("zstd no pudo descomprimir la fuente de uso"); }
            let count = count.map_err(|_| "error al leer zstd")?;
            offset = count.saturating_sub(INITIAL_TAIL);
            if offset > 0 { discard_first = true; acc.partial(); }
            file.seek(SeekFrom::Start(0)).map_err(|_| "no se pudo posicionar la fuente de uso")?;
        }
        // El path no va en argv: solo se entrega al proceso el descriptor de lectura.
        let mut child = start_zstd("zstd", file)?;
        let mut out = child.stdout.take().ok_or("no se pudo leer zstd")?;
        let skipped = io::copy(&mut (&mut out).take(offset), &mut io::sink()).map_err(|_| "error al leer zstd")?;
        if skipped != offset { let _ = child.kill(); let _ = child.wait(); return Err("el transcript comprimido cambió inesperadamente"); }
        let read = scan(BufReader::new(out), &mut acc, t.agent, discard_first);
        let status = child.wait().map_err(|_| "no se pudo esperar a zstd")?;
        if !status.success() { return Err("zstd no pudo descomprimir la fuente de uso"); }
        offset = offset.saturating_add(read.map_err(|_| "error al leer zstd")?);
    } else {
        file.seek(SeekFrom::Start(offset)).map_err(|_| "no se pudo posicionar la fuente de uso")?;
        offset = offset.saturating_add(scan(BufReader::new(file), &mut acc, t.agent, discard_first).map_err(|_| "error al leer la fuente de uso")?);
    }
    t.accum = acc; t.cursor = Some(Cursor { dev: meta.dev(), inode: meta.ino(), size, offset, mtime }); Ok(())
}
fn start_zstd(program: &str, file: File) -> Result<std::process::Child, &'static str> {
    Command::new(program).arg("-dc").stdin(Stdio::from(file)).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
        .map_err(|e| if e.kind() == io::ErrorKind::NotFound { "zstd no está instalado o no está en PATH" } else { "no se pudo iniciar zstd" })
}
/// Devuelve bytes hasta el último salto de línea completo. No conserva fragmentos de texto entre polls.
fn scan(mut reader: impl BufRead, acc: &mut Accum, agent: Agent, mut discard: bool) -> io::Result<u64> {
    let mut committed = 0u64; let mut pending = 0u64; let mut line = Vec::new(); let mut too_long = false;
    loop {
        let (len, end) = {
            let data = reader.fill_buf()?; if data.is_empty() { break; }
            let end = data.iter().position(|&b| b == b'\n'); let len = end.map_or(data.len(), |i| i+1);
            if !discard && !too_long {
                if line.len().saturating_add(len) > MAX_LINE { line.clear(); too_long = true; }
                else { line.extend_from_slice(&data[..len]); }
            }
            (len, end.is_some())
        };
        reader.consume(len); pending += len as u64;
        if end {
            committed += pending; pending = 0;
            if discard { discard = false; }
            else if too_long { acc.partial(); }
            else { acc.line(agent, &line); }
            too_long = false; line.clear();
        }
    }
    Ok(committed)
}
