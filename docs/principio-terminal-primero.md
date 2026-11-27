# Principio de producto: terminal primero, orquestación opcional

Dicho por el usuario: galgo debe ser una TERMINAL rápida, ligera y muy estética como Rex, con opciones avanzadas; la orquestación de agentes es una ventaja que se usa si se quiere, no la cara principal. Hoy la GUI se ve "muy orquestador y poco terminal".

## Reglas de diseño
1. **Abre y ya es un terminal**: al lanzar la app aparece un shell en ~ (o el último estado). Sin pantalla de bienvenida, sin "sin workspace".
2. **Chrome mínimo por defecto**: barra superior con píldoras (una por terminal/sesión, con icono, proceso y cwd como Rex) y el terminal ocupando todo. Sin sidebar, sin miga, sin barra de estado permanentes. Se muestran con atajo (Cmd+B sidebar, Cmd+/ barra de estado) o cuando hacen falta.
3. **Divulgación progresiva de la orquestación**: el panel `agents`, las cuotas, el uso y los proyectos/ramas solo aparecen cuando hay un agente real (hook o galgo spawn/MCP) o el usuario abre el sidebar. Cuando corres `claude` a mano en un shell, la píldora de esa pestaña gana un punto de estado (trabajando/bloqueado/listo) y un contador; nada más.
4. **Un solo nivel de pestañas** + splits dentro (como una terminal normal). Las "sesiones" (proyectos) son agrupaciones opcionales, no una segunda fila obligatoria. La segunda fila de pestañas y la miga `proyecto / rama` desaparecen cuando solo hay una.
5. **Lo básico de una terminal se hace bien**: Cmd+C/V, zoom de fuente Cmd+/-/0, búsqueda en el scrollback Cmd+F, clic en URLs y rutas, arrastrar archivos para pegar su ruta, título de pestaña desde cwd/proceso, Cmd+K limpiar, scroll suave, selección por palabra/línea (doble/triple clic), Cmd+T/D/W conocidos, restauración de pestañas.
6. **Ligera y rápida**: arranque < 200 ms hasta el primer prompt, CPU ~0% en reposo, latencia de tecla < 16 ms. Cambiar el sondeo de snapshot cada 50 ms por suscripción empujada (el daemon avisa cuando cambia la pantalla de un panel visible), sin tocar paneles ocultos.
7. **Estética Rex**: tema cálido, esquinas redondeadas, tipografía proporcional en el chrome, mono en el terminal, mucho aire, sin líneas duras.
8. **Las opciones avanzadas viven en la paleta (Cmd+K) y en Ajustes**: lanzar agente, nuevo worktree, workspaces, hooks, uso.
