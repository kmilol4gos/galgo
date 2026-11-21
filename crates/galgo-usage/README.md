# galgo-usage

Librería local y síncrona: no inicia hilos, no abre red ni necesita tokio. Los parsers filtran líneas por bytes y serde ignora contenidos ajenos a sus structs numéricos/enumerados; los IDs de deduplicación se convierten inmediatamente en hashes, y el identificador de sesión tampoco se conserva como texto. Las rutas solo se mantienen en memoria. No se registran líneas ni fragmentos de errores. Esta fase no configura hooks, statusline, daemon ni GUI.

```rust,no_run
use galgo_usage::{Agent, UsageTracker};
let mut tracker = UsageTracker::new();
tracker.track("panel", Agent::Claude, "session-id", "/ruta/al/transcript.jsonl");
if let Some(usage) = tracker.poll("panel") {
    println!("entrada: {}, coste: {:?}", usage.input, usage.cost_usd);
}
if let Some(error) = tracker.last_error("panel") { eprintln!("{error}"); }
let limits = tracker.limits("panel");
# let _ = limits;
```

`track` repetido con la misma fuente conserva offsets; si cambia la sesión, el agente o la ruta, reinicia. `poll` lee únicamente líneas completas desde el último offset y detecta reemplazo de inode o reducción del archivo. Devuelve `None` si aún no hay métricas; si ya había métricas, un error posterior mantiene el último resultado y se puede consultar por `last_error`. `untrack` olvida la fuente. Un archivo de texto sin offset previo y mayor de 100 MiB se lee desde la cola, con `partial=true` y coste desconocido; los totales parciales no representan el histórico. Líneas de más de 8 MiB se omiten y también marcan `partial`. Nunca se guardan fragmentos entre polls.

Para Codex `.jsonl.zst` se invoca `zstd -dc` con el archivo por stdin, no en argv. Si falta `zstd`, `last_error` devuelve un mensaje explícito. Puesto que el formato comprimido no permite seek descomprimido, cada cambio del archivo exige descomprimir desde el principio y descartar el prefijo ya procesado. Si un archivo comprimido inicial ocupa más de 100 MiB, se descomprime una primera vez sin guardar el flujo para medirlo y una segunda vez para procesar solo la cola; se marca `partial`. Esto no evita el coste de descomprimir el archivo completo. Con archivo sin cambios no se vuelve a invocar `zstd`.

Claude suma tokens por ID de mensaje o, si falta, por ID de solicitud; el último `isSidechain=false` aporta contexto en tokens. No se supone una ventana. PI suma el coste declarado por el agente; no se infiere ventana. Codex toma los totales acumulados sin volverlos a sumar, y estima contexto como última entrada más salida: la fórmula exacta de Codex sigue sin verificar. Sus cuotas se exponen mediante `limits` con `observed_at` en segundos Unix del momento en que se leen; al procesar históricos ese instante **no es la fecha original de la cuota**. No hay cuotas de Claude sin statusline ni cuotas de PI verificadas.

`prices.toml` es una tabla versionada incluida con `include_str!`; cada fila tiene fuente y fecha según `docs/usage-plan.md`. Se usan solo IDs reconocidos y verificados. Los importes de Claude y Codex son **equivalentes de lista**, no facturas; tarifas desconocidas, desglose de caché insuficiente, sobrecargos no verificados y precios Codex por encima de 272k producen `cost_source=Unknown`. Las filas Haiku con umbral no se aplican hasta verificar su regla. `CostSource::Statusline` está reservado para una fase posterior.
