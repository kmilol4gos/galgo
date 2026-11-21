# Plan técnico: GUI de galgo

**Alcance:** macOS primero, Linux después; varias ventanas son clientes del mismo daemon. Investigación: 20-11-2026; documentación upstream consultada, no compilada ni probada localmente. Versiones efectivamente publicadas y comportamiento de cada versión: **no verificados**. Fijar versiones/revisiones tras el prototipo, no usar `*` en producción.

## 1. UI y render

| Opción | Estado y compromiso para galgo |
|---|---|
| **GPUI** | [README upstream](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md): uso independiente documentado con crates `gpui` y `gpui_platform`; macOS/Metal y Linux/Wayland o X11. Pre-1.0, breaking changes frecuentes. Adecuado para UI compleja y render acelerado; integración de una cuadrícula terminal, IME y accesibilidad: **no verificados**. |
| **wgpu + winit** | [wgpu](https://github.com/gfx-rs/wgpu) ofrece Metal y Vulkan; [winit](https://github.com/rust-windowing/winit) ventanas/eventos multiplataforma. Control máximo, pero habría que construir o integrar widgets, texto, foco y accesibilidad. Estabilidad de API y compatibilidad de versiones concretas: **no verificadas**; el README de winit muestra una beta. |
| **iced** | [Upstream](https://github.com/iced-rs/iced): macOS/Linux, widgets, render wgpu o software; se describe como experimental. Buena alternativa productiva; eficiencia e integración del widget terminal, IME y estabilidad de API concreta: **no verificadas**. |
| **egui** | [Upstream](https://github.com/emilk/egui): immediate mode, `eframe` soporta macOS/Linux. Prototipado sencillo; fidelidad nativa, texto complejo, accesibilidad y coste de repintado de terminal: **no verificados**. Estabilidad de API concreta: **no verificada**. |

**Recomendación provisional: GPUI**, por UI de escritorio y render GPU integrados; encapsular el widget terminal y el transporte para poder migrar. Lo descartaría si el corte vertical no cumple latencia, IME/accesibilidad indispensables o compilación reproducible en macOS y Linux; alternativa: iced, y wgpu+winit solo si se justifica mantener la infraestructura propia. Ninguna opción garantiza apariencia nativa sin trabajo adicional.

## 2. Terminal: una sola autoridad

El daemon mantiene PTY, parser, modos, pantalla y scrollback; cada GUI mantiene una réplica de celdas para dibujar, selección local y métricas de fuente. El daemon genera respuestas VT al PTY una sola vez; no reproducir bytes PTY en cada GUI como fuente autoritativa. La entrada se codifica según los modos vigentes **en el daemon**, a partir de eventos de tecla/ratón del cliente. Texto compuesto/IME llega como texto comprometido. Definir dimensiones, Unicode/ancho de celda, atributos, cursor, pantalla alternativa, modos e identificadores de enlaces en el modelo de pantalla.

- **vt100 actual:** mínimo para el primer corte; `Read` devuelve texto sin estilos. Soporte real de kitty keyboard, synchronized output, OSC 8 hyperlinks y mouse SGR en la versión instalada: **no verificado**; no anunciar capacidades sin pruebas.
- **alacritty_terminal:** candidato Rust para sustituir el parser en el daemon; evita ejecutar otra terminal por cliente. Cobertura de esos cuatro protocolos, API y posibilidad de exportar estado/diffs en la versión elegida: **no verificadas**; evaluar fixtures y mantenimiento antes de adoptar.
- **libghostty-vt:** [cabecera upstream](https://github.com/ghostty-org/ghostty/blob/main/include/ghostty/vt.h) documenta estado, estilos, callbacks y render state; declara ABI C estable desde 1.0. Versión publicada, cobertura de los cuatro protocolos y empaquetado Zig/C en macOS/Linux: **no verificados**. FFI y distribución aumentan el coste.

Decisión: conservar vt100 solo para el corte inicial; evaluar alacritty_terminal y libghostty-vt con la misma suite antes de prometer protocolos modernos. Probar específicamente negociación kitty, agrupación de synchronized output, apertura segura de hyperlinks y coordenadas/modificadores SGR; desactivar o no anunciar lo que falle. Política de portapapeles OSC 52 y enlaces: consentimiento explícito, nunca ejecución automática.

## 3. Daemon y protocolo

Hoy el socket Unix usa JSON por línea, una respuesta por solicitud; `Read` entrega texto y `Send` no modela teclas. El lector PTY actual procesa bytes sin publicar eventos. Añadir protocolo versionado sin romper CLI: conexión GUI negociada, sobres con `request_id` y mensajes `response`/`event`; una única tarea escritora serializa mensajes. Conexiones antiguas conservan su contrato. Autorización: socket 0600 existente y límites de tamaño; validar IDs y datos de entrada.

Nuevas variantes `Request` propuestas:
- `Hello { version, capabilities }`: negociar versión, codificaciones y límites.
- `Subscribe { panes, topics }` / `Unsubscribe { subscription_id }`: eventos de pantalla, ciclo de vida, estado e inbox; suscripción global para barra lateral y atención.
- `GetSnapshot { pane }`: cuadrícula, atributos, cursor, modos, tamaño, scrollback paginado o metadatos y `(epoch, seq)`; `GetScrollback { pane, before, limit }` para historia.
- `Input { pane, events }`: teclas con modificadores/fase, texto comprometido, pegar y ratón con coordenadas de celda; validar modo y serializar escrituras al PTY. Respuesta confirma aceptación, no presentación.
- `AcquireControl { pane }`, `ReleaseControl { pane }`, `Resize { pane, rows, cols, pixel_width, pixel_height }`: un controlador por panel; solo él envía entrada y cambia tamaño de PTY **y parser**, ordenados con el procesamiento de salida. Observadores escalan o recortan sin cambiar el tamaño. Al desconectar, liberar control; solicitarlo de nuevo explícitamente.

`Subscribe` registra el observador y captura snapshot/seq **atómicamente** respecto de la salida: entrega snapshot seguido de diffs consecutivos; `GetSnapshot` permite resincronizar. Diffs incluyen celdas, cursor, modos y cambios de tamaño; `epoch` cambia al reiniciar o reemplazar pantalla. Hueco, cambio de epoch, reconexión o cola llena ⇒ invalidar réplica y pedir snapshot nuevo; colas acotadas por cliente, nunca bloquear lectura PTY por una GUI lenta. Conservar eventos de estado mediante snapshot inicial y secuencia, no depender de notificaciones perdibles. Mantener IDs de panel estables durante la vida del daemon.

## 4. Corte vertical y medición

En macOS: abrir una ventana, `Spawn` de shell en un panel, adquirir control y suscribirse; escribir texto y Enter, ver eco, cursor y colores ANSI; resize actualiza PTY y cuadrícula; cerrar GUI no mata shell y reconectar restaura pantalla. Sin sidebar ni splits. Test automatizado con proceso PTY determinista que emite ANSI y eco, más prueba manual en shell real.

Medir en el mismo Mac: tiempo monótono desde recepción del evento de tecla en GUI hasta presentación del frame que contiene el eco identificado (instrumentar ID y marcador de prueba; no confundir confirmación de escritura con presentación). Registrar p50/p95/p99 de ≥1.000 teclas, hardware, refresh, carga y build release; objetivo inicial p95 < 50 ms en reposo, sin pérdidas. Si no se puede observar presentación real, reportar «envío a GPU», no latencia a pantalla; validar aparte con cámara de alta velocidad.

## 5. Riesgos e hitos

**Riesgos, por prioridad:** (1) semántica VT incompleta/corrupción de estado; (2) carreras, pérdida de diffs y clientes lentos; (3) entrada, IME y control de tamaño compartido; (4) latencia/render y consumo con muchos paneles; (5) cambios GPUI, accesibilidad y distribución Linux; (6) seguridad de socket, enlaces y portapapeles.

1. **Spike UI:** compilar ventana GPUI en macOS y Linux (Wayland o X11 documentado), probar IME y registrar limitaciones; fijar revisión o cambiar a iced.
2. **Corte vertical:** cumplir prueba y medición de §4; test de colores, cursor, resize y reconexión pasa.
3. **Sincronización multicliente:** dos GUIs ven idéntica cuadrícula y estado; prueba fuerza pérdida/desborde y reconexión, verifica recuperación y que observador no redimensiona ni escribe.
4. **Compatibilidad terminal:** suite reproducible para los cuatro protocolos de §2, Unicode, scrollback y pantalla alternativa; publicar matriz de resultados y elegir motor; no anunciar funciones fallidas.
5. **Producto:** sidebar/proyectos, bandeja de atención y splits; test de estados blocked/done y dos ventanas sobre misma sesión; pruebas de accesibilidad y empaquetado en ambos sistemas, con limitaciones documentadas.
