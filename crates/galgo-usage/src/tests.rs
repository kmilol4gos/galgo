use super::*;
use std::{fs, io::Write, sync::atomic::{AtomicU64, Ordering}};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp { fn new() -> Self { let p = std::env::temp_dir().join(format!("galgo-usage-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))); fs::create_dir_all(&p).unwrap(); Self(p) } fn file(&self, name: &str) -> PathBuf { self.0.join(name) } }
impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
fn append(path: &Path, text: &str) { let mut f = fs::OpenOptions::new().create(true).append(true).open(path).unwrap(); f.write_all(text.as_bytes()).unwrap(); }
fn close(a: f64, b: f64) { assert!((a-b).abs() < 1e-9, "{a} != {b}"); }

#[test]
fn claude_deduplicates_and_tracks_root_and_both_cache_prices() {
    let dir = Temp::new(); let path = dir.file("claude.jsonl");
    let a = r#"{"type":"assistant","isSidechain":false,"requestId":"request-1","message":{"id":"msg-1","model":"claude-opus-5-5","content":[{"text":"SECRET_TEST_ONLY"}],"usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":30,"cache_creation_input_tokens":20,"cache_creation":{"ephemeral_5m_input_tokens":5,"ephemeral_1h_input_tokens":15},"output_tokens_details":{"thinking_tokens":2}}}}"#;
    let b = r#"{"type":"assistant","isSidechain":true,"message":{"id":"msg-2","model":"claude-opus-5-5","usage":{"input_tokens":7,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;
    append(&path, &format!("{a}\n{a}\n{b}\n"));
    let mut tracker=UsageTracker::new(); tracker.track("w",Agent::Claude,"session",&path);
    let u=tracker.poll("w").unwrap(); assert_eq!((u.input,u.output,u.cache_read,u.cache_write,u.reasoning),(107,11,30,20,2));
    assert_eq!((u.cache_write_5m,u.cache_write_1h),(5,15)); assert_eq!(u.context_tokens,Some(150)); assert_eq!(u.context_window,None); assert_eq!(u.context_pct,None);
    assert_eq!(u.cost_source,CostSource::ListPrice);
    close(u.cost_usd.unwrap(), (107.0*4.0+11.0*20.0+30.0*0.2+5.0*5.0+15.0*8.0)/1_000_000.0);
    tracker.track("w",Agent::Claude,"session",&path); assert_eq!(tracker.poll("w").unwrap().input,107);
    let fallback = r#"{"type":"assistant","isSidechain":false,"requestId":"req-fallback","message":{"model":"unknown-user-text","usage":{"input_tokens":2,"output_tokens":1}}}"#;
    append(&path,&format!("{fallback}\n{fallback}\n")); let u=tracker.poll("w").unwrap();
    assert_eq!(u.input,109); assert_eq!(u.cost_source,CostSource::Unknown); assert_eq!(u.cost_usd,None);
}

#[test]
fn codex_cumulative_context_limits_and_known_price() {
    let dir=Temp::new(); let path=dir.file("codex.jsonl");
    let model=r#"{"type":"turn_context","payload":{"model":"gpt-5.6-terra","creator_account_id":"PRIVATE"}}"#;
    let event=r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100,"reasoning_output_tokens":20},"last_token_usage":{"input_tokens":800,"cached_input_tokens":200,"output_tokens":50},"model_context_window":10000},"rate_limits":{"plan_type":"plus","primary":{"window_minutes":300,"used_percent":12.0,"resets_at":2000000000},"secondary":{"window_minutes":10080,"used_percent":2.0},"credits":{"balance":"PRIVATE"}}}}"#;
    append(&path,&format!("{model}\n{event}\n{event}\n"));
    let mut t=UsageTracker::new(); t.track("p",Agent::Codex,"s",&path); let u=t.poll("p").unwrap();
    assert_eq!((u.input,u.cache_read,u.output,u.reasoning),(600,400,100,20));
    assert_eq!(u.context_tokens,Some(850)); assert_eq!(u.context_window,Some(10000)); assert_eq!(u.context_pct,Some(8.5));
    assert_eq!(u.model.as_deref(),Some("gpt-5.6-terra")); assert_eq!(u.cost_source,CostSource::ListPrice);
    close(u.cost_usd.unwrap(),(600.0*2.0+400.0*0.2+100.0*12.0)/1_000_000.0);
    let limits=t.limits("p").unwrap(); assert_eq!(limits.plan.as_deref(),Some("plus")); assert_eq!(limits.windows.len(),2); assert_eq!(limits.windows[0].minutes,300); assert!(limits.observed_at>0);
    append(&path,"{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":1100,\"cached_input_tokens\":400,\"output_tokens\":100},\"last_token_usage\":{\"input_tokens\":300000,\"output_tokens\":1}}}}\n");
    assert_eq!(t.poll("p").unwrap().cost_source, CostSource::Unknown);
}

#[test]
fn pi_cost_and_never_retains_conversation_or_identifiers() {
    let dir=Temp::new(); let path=dir.file("pi.jsonl");
    let line=r#"{"type":"message","id":"SECRET_SESSION_ID","message":{"role":"assistant","model":"SECRET_PRIVATE_MODEL","content":[{"type":"text","text":"SECRET_MESSAGE_BODY"}],"usage":{"input":10,"output":3,"reasoning":1,"cacheRead":5,"cacheWrite":2,"cost":{"total":0.123,"totalFromProvider":false}},"email":"SECRET_PRIVATE_EMAIL"}}"#;
    append(&path,&format!("{line}\n")); let mut t=UsageTracker::new(); t.track("p",Agent::Pi,"SECRET_SESSION_ID",&path);
    let u=t.poll("p").unwrap(); assert_eq!((u.input,u.output,u.cache_read,u.cache_write,u.reasoning),(10,3,5,2,1)); assert_eq!(u.context_tokens,Some(17));
    assert_eq!(u.cost_source,CostSource::Agent); close(u.cost_usd.unwrap(),0.123); assert_eq!(u.model,None);
    let serialized=serde_json::to_string(&u).unwrap(); assert!(!serialized.contains("SECRET")); assert!(!format!("{:?}",u).contains("SECRET"));
    let parsed: PiLine=serde_json::from_str(line).unwrap(); assert!(!format!("{parsed:?}").contains("SECRET"));
    let claude: ClaudeLine=serde_json::from_str(r#"{"type":"assistant","requestId":"SECRET_ID","message":{"id":"SECRET_ID","model":"SECRET_MODEL","usage":{"input_tokens":1,"output_tokens":1},"content":"SECRET_BODY"}}"#).unwrap();
    assert!(!format!("{claude:?}").contains("SECRET"));
    let codex: CodexLine=serde_json::from_str(r#"{"type":"event_msg","payload":{"type":"token_count","creator_account_id":"SECRET_ID","rate_limits":{"credits":{"balance":"SECRET"}}}}"#).unwrap();
    assert!(!format!("{codex:?}").contains("SECRET"));
}

#[test]
fn incremental_partial_line_truncate_replace_and_retrack() {
    let dir=Temp::new(); let path=dir.file("pi.jsonl");
    let l=r#"{"type":"message","message":{"role":"assistant","usage":{"input":7,"output":2,"cost":{"total":1.0}}}}"#;
    append(&path,l); let mut t=UsageTracker::new(); t.track("p",Agent::Pi,"first",&path); assert!(t.poll("p").is_none());
    append(&path,"\n"); assert_eq!(t.poll("p").unwrap().input,7); assert_eq!(t.poll("p").unwrap().input,7);
    append(&path,&format!("{l}\n")); assert_eq!(t.poll("p").unwrap().input,14);
    fs::write(&path,format!("{l}\n")).unwrap(); assert_eq!(t.poll("p").unwrap().input,7);
    let replacement=dir.file("replacement"); fs::write(&replacement,format!("{l}\n{l}\n")).unwrap(); fs::rename(&replacement,&path).unwrap(); assert_eq!(t.poll("p").unwrap().input,14);
    t.track("p",Agent::Pi,"second",&path); assert_eq!(t.poll("p").unwrap().input,14);
    t.untrack("p"); assert!(t.poll("p").is_none());
}

#[test]
fn oversized_initial_file_marks_totals_partial_without_storing_text() {
    let dir=Temp::new(); let path=dir.file("large.jsonl");
    let mut f=fs::File::create(&path).unwrap(); f.set_len(INITIAL_TAIL+1).unwrap();
    use std::io::Seek; f.seek(SeekFrom::End(0)).unwrap(); writeln!(f,"\n{{\"type\":\"message\",\"message\":{{\"role\":\"assistant\",\"usage\":{{\"input\":1,\"output\":1,\"cost\":{{\"total\":1}}}}}}}}").unwrap();
    let mut t=UsageTracker::new(); t.track("p",Agent::Pi,"s",&path); let u=t.poll("p").unwrap(); assert!(u.partial); assert_eq!(u.cost_source,CostSource::Unknown); assert_eq!(u.context_tokens,Some(1));
}

#[test]
fn missing_zstd_error_is_clear_and_private() {
    let dir = Temp::new();
    let path = dir.file("PRIVATE-TRANSCRIPT.zst");
    fs::write(&path, b"not important").unwrap();
    let result = start_zstd("/galgo-test-no-such-zstd", File::open(path).unwrap());
    assert_eq!(result.err(), Some("zstd no está instalado o no está en PATH"));
}

#[test]
fn zstd_fixture_and_errors_are_safe() {
    if Command::new("zstd").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() { eprintln!("zstd no disponible: se omite compresión real"); return; }
    let dir=Temp::new(); let source=dir.file("c.jsonl"); let path=dir.file("c.jsonl.zst");
    fs::write(&source,"{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.6-terra\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":10,\"output_tokens\":2},\"last_token_usage\":{\"input_tokens\":10,\"output_tokens\":2},\"model_context_window\":100}}}\n").unwrap();
    assert!(Command::new("zstd").args(["-q","-f"]).arg(&source).arg("-o").arg(&path).status().unwrap().success());
    let mut t=UsageTracker::new(); t.track("p",Agent::Codex,"s",&path); let u=t.poll("p").unwrap(); assert_eq!(u.input,10); assert_eq!(u.context_pct,Some(12.0)); assert_eq!(t.poll("p").unwrap().input,10);
    fs::write(&path,"no es zstd SECRET").unwrap(); assert!(t.poll("p").is_some()); assert_eq!(t.last_error("p"),Some("zstd no pudo descomprimir la fuente de uso")); assert!(!t.last_error("p").unwrap().contains("SECRET"));
}

/// Fixture sintético con el id real de modelo (`claude-sonnet-5-5`). Coste a mano:
/// (1000·2 + 500·0,10 + 200·10 + 100·2,50) / 10⁶ = 0,0043 $ (fila «Sonnet 5.5» de prices.toml).
#[test]
fn claude_sonnet_55_extrae_modelo_contexto_y_precio_de_lista() {
    let dir = Temp::new(); let path = dir.file("sonnet.jsonl");
    let a = r#"{"type":"assistant","isSidechain":false,"requestId":"req-s1","message":{"id":"msg-s1","model":"claude-sonnet-5-5","usage":{"input_tokens":1000,"output_tokens":200,"cache_read_input_tokens":500,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":0}}}}"#;
    append(&path, &format!("{a}\n"));
    let mut t = UsageTracker::new(); t.track("p", Agent::Claude, "s", &path);
    let u = t.poll("p").unwrap();
    assert_eq!(u.model.as_deref(), Some("claude-sonnet-5-5"));
    assert_eq!(u.cost_source, CostSource::ListPrice);
    close(u.cost_usd.unwrap(), 0.0043);
    // Contexto en tokens absolutos, sin ventana inventada.
    assert_eq!(u.context_tokens, Some(1600));
    assert_eq!(u.context_window, None);
    assert_eq!(u.context_pct, None);
}

/// Un modelo sin fila verificada conserva su nombre, pero su coste queda desconocido (no se inventa).
#[test]
fn modelo_sin_precio_verificado_muestra_id_sin_coste() {
    let dir = Temp::new(); let path = dir.file("sonnet5.jsonl");
    let a = r#"{"type":"assistant","isSidechain":false,"requestId":"r","message":{"id":"m","model":"claude-sonnet-5","usage":{"input_tokens":10,"output_tokens":1}}}"#;
    append(&path, &format!("{a}\n"));
    let mut t = UsageTracker::new(); t.track("p", Agent::Claude, "s", &path);
    let u = t.poll("p").unwrap();
    assert_eq!(u.model.as_deref(), Some("claude-sonnet-5"));
    assert_eq!(u.cost_usd, None);
    assert_eq!(u.cost_source, CostSource::Unknown);
}

/// Un mensaje posterior de un modelo desconocido no borra el modelo conocido del panel.
#[test]
fn modelo_desconocido_no_borra_el_ultimo_conocido() {
    let dir = Temp::new(); let path = dir.file("mixto.jsonl");
    let a = r#"{"type":"assistant","isSidechain":false,"requestId":"r1","message":{"id":"m1","model":"claude-sonnet-5-5","usage":{"input_tokens":10,"output_tokens":1}}}"#;
    let b = r#"{"type":"assistant","isSidechain":false,"requestId":"r2","message":{"id":"m2","model":"modelo-nuevo-9","usage":{"input_tokens":10,"output_tokens":1}}}"#;
    append(&path, &format!("{a}\n{b}\n"));
    let mut t = UsageTracker::new(); t.track("p", Agent::Claude, "s", &path);
    let u = t.poll("p").unwrap();
    assert_eq!(u.model.as_deref(), Some("claude-sonnet-5-5"));
    assert_eq!(u.cost_usd, None, "un precio desconocido hace desconocido el coste total");
}

/// Un id de modelo con caracteres raros se descarta; la línea sigue contando tokens y no se marca parcial.
#[test]
fn id_de_modelo_mal_formado_se_descarta_sin_invalidar_la_linea() {
    let dir = Temp::new(); let path = dir.file("raro.jsonl");
    let a = r#"{"type":"assistant","isSidechain":false,"requestId":"r1","message":{"id":"m1","model":"<script> x","usage":{"input_tokens":7,"output_tokens":1}}}"#;
    append(&path, &format!("{a}\n"));
    let mut t = UsageTracker::new(); t.track("p", Agent::Claude, "s", &path);
    let u = t.poll("p").unwrap();
    assert_eq!(u.model, None);
    assert_eq!(u.input, 7);
    assert!(!u.partial);
}
