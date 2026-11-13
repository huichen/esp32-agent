# esp32-agent

[English](README.md) · [简体中文](README.zh.md) · [Français](README.fr.md) · **Español** · [Русский](README.ru.md) · [العربية](README.ar.md) · [日本語](README.ja.md) · [한국어](README.ko.md)

Firmware para un ESP32 que ejecuta su propio bucle de agente y sirve un endpoint MCP, de modo
que un modelo puede gobernar la placa directamente y cualquier agente externo puede usar las
mismas herramientas por HTTP.

Se graba, se conecta a la WiFi y sondea los buses para los que tiene drivers. Cada dispositivo
que responde se convierte en herramientas MCP. El agente de la placa ve esas mismas
herramientas y responde con ellas; se puede declarar un sensor nuevo en JSON con la placa en
marcha.

> El bucle de agente sigue a [FutureOS](https://github.com/futuregene/future-os): transcripción,
> diario de herramientas, eventos de flujo, adaptadores de protocolo, índice de memoria.

| Qué | En detalle |
|---|---|
| Un agente se ejecuta en la placa | El bucle, las herramientas, la memoria y la transcripción son firmware; el modelo es cualquier endpoint compatible con OpenAI o de Anthropic. [Cómo darle un modelo](docs/model.md) |
| El conjunto de herramientas es un documento | `manifest` (`command: apply`) valida un manifiesto JSON, lo compila en herramientas y está activo en el mismo turno. [Manifiesto](docs/manifest.md) |
| Encuentra su propio hardware | I2C, 1-Wire y los pines declarados se sondean al arrancar; solo lo que responde obtiene herramientas. [Capacidades](docs/capabilities.md) |
| Un conjunto de herramientas, dos invocadores | El endpoint MCP y el agente de la placa ven las mismas herramientas, desde una sola descripción. [API HTTP y MCP](docs/http-api.md) |
| Una consola sobre HTTPS | Hablar con el modelo y configurar dispositivos, herramientas, programación, registros, WiFi y el modelo desde un teléfono; la placa emite su propio certificado. [Consola web](docs/web-console.md) |
| Actúa sin nadie delante | Prompts temporizados y reglas de sensores que lanzan un prompt cuando una lectura cruza un umbral. [Trabajo programado](docs/scheduling.md) |
| Un shell y un lenguaje de script | `shell` con 57 comandos con nombres de NuttX, y un intérprete para bucles sobre llamadas a herramientas. [Shell](docs/shell.md) · [Script](docs/scripting.md) |
| Recuerda lo que se le dijo | Órdenes permanentes, `/AGENTS.md`, un índice `/MEMORY.md` y habilidades, ensamblados en el prompt en cada turno. [Lo que el modelo ve en cada turno](docs/context.md) |
| Habilidades incluidas | Una sola rutina viene en el firmware, `create-skill`, con la cabecera estándar `---`, y enseña el resto: qué es una habilidad y cómo escribir una. Aterriza en `/skills` en el primer arranque, y tu edición se queda. [Habilidades](docs/agent-skills.md) |
| La placa es el servidor | HTTPS desde la placa, un certificado que genera ella misma, un PIN de consola y un token MCP independiente. [Autenticación](docs/security.md) |
| Comprobable sin hardware | El simulador ejecuta el mismo `agent-core` contra una placa virtual. [Simulador](docs/simulator.md) |

## Grabar y conectar

Compila desde el código fuente; no se publica una imagen de versión precompilada. El objetivo
predeterminado es un ESP32-S3 N16R8 con 16 MiB de flash y 8 MiB de PSRAM octal. Instala las
herramientas de [Grabación](docs/flashing.md) y ejecuta:

```bash
bash scripts/build-firmware.sh --flash --port /dev/ttyACM0
```

En Windows, usa `powershell -File scripts/build-firmware.ps1 -Flash -Port COM3`. Los scripts
compilan antes de grabar y pasan explícitamente la tabla de particiones y la partición de
aplicación `factory`.

La placa no tiene credenciales WiFi, así que levanta un punto de acceso llamado
`esp32-agent-setup`. Únete, abre `http://192.168.4.1/`, escribe el nombre de la red y la
contraseña, guarda. Van a NVS y la placa se reconecta. Hasta entonces no alcanza nada fuera de
la red local: ni el endpoint del modelo, ni un espejo de paquetes.

## Uso

**En un navegador**: `http://<board>/` (o `https://<board>/`) es la consola. Hablar con el
modelo, listar los dispositivos encontrados, activar o apagar herramientas, leer el registro,
poner el PIN.

**Desde un agente**: `GET http://<board>/agent.json` devuelve la interfaz en JSON: cada ruta
con su método, cuerpo y autenticación, la superficie MCP, la tabla de ajustes con tipos y
valores por defecto, y los límites. Un agente que sepa pedir una URL no necesita nada más.

**Una placa nueva ya sabe aprender.** `/skills/create-skill` es la única rutina incluida: dice qué es una habilidad, la cabecera `---` que debe llevar y cómo escribir una que la próxima sesión encuentre. Todo lo demás —el cableado de tu mesa, qué hacer cuando una placa parece muerta— es una habilidad que el agente escribe y conserva. Es un archivo normal del espacio de trabajo, instalado en el primer arranque donde la ruta esté vacía; un restablecimiento de fábrica lo repone y `skills.seed_defaults=false` lo impide. La primera frase útil para una placa recién flasheada puede ser «apunta tu cableado».

## Dale un modelo

Abre `http://<board>/`, panel **Agent**, escribe el endpoint, el id del modelo y la clave, y
guarda. La clave es de solo escritura: va a NVS y las lecturas muestran `api_key_set`, no la clave.
El mismo panel tiene
`protocol` (`chat`, `responses` o `anthropic`), `reasoning`, `context_tokens` y
`max_output_tokens`.

**O deja que lo haga un agente.** Cuando un agente ha pedido `agent.json` ya conoce estos
ajustes, sus tipos y sus valores por defecto, así que basta una instrucción:

> apunta la placa a `https://your-provider/v1/chat/completions`, modelo `…`, clave `sk-…`,
> razonamiento `low`, contexto `64000`, salida `4096`

El agente los aplica con `POST /api/agent/config` —la misma llamada que hace el panel— y vuelve
a leer la configuración para confirmar. No hace falta escribir nada en una terminal, y la misma
instrucción sirve para cada placa que pueda alcanzar. Sin ninguna clave, `base_url` en
`http://127.0.0.1/selftest/chat/completions` hace que el bucle corra contra la propia placa.

## Notas de seguridad

- **Las credenciales se quedan en la placa.** El WiFi va a NVS por `provision`, nunca a la
  imagen; `api_key` es de solo escritura y las lecturas solo muestran `api_key_set`.
- **Configura ambas credenciales antes de que la placa salga de tu mesa.** `auth.portal_pin`
  protege las escrituras REST y las lecturas de configuración, programación, transcripción, prompt, espacio
  de trabajo, adjuntos, registros, diario de ejecución y resultados de scripts. MCP usa
  `auth.mcp_token`; si no está configurado, recurre a la credencial de la consola.
- **Compara el certificado.** El de la placa es autofirmado; su huella está en `GET /api/tls`.
  El puerto 80 sigue abierto por defecto: un certificado malo cuesta una URL, no la placa; `mcp.plain_http: false` lo cierra, y entonces el cable USB es la única vuelta atrás.
- **`shell`, `script` y las herramientas del manifiesto mueven pines.** Ese es el objetivo de la
  placa; configura la credencial de cada interfaz para distinguir a los invocadores autorizados de los transeúntes.

[Security](docs/security.md) dice qué no se defiende a propósito.

## Estado

| Parte | Estado |
|---|---|
| `agent-core` | 831 pruebas unitarias y 5 de integración pasan; `scripts/smoke.ps1` pasa de extremo a extremo |
| Simulador | la consola, MCP y el bucle de agente funcionan de extremo a extremo |
| Firmware | firmware histórico del banco en una ESP32-S3-DevKitC-1 N16R8 (IDF v5.5.2): 42 herramientas, OLED 128×32, micrófono y amplificador I²S, WS2812, consola TLS, turnos del modelo por HTTPS; cambios de la auditoría no verificados en la placa |
| No implementado / no verificado | OTA y captura DVP del firmware no implementados. Los drivers SD de sectores sin procesar y DHT11/22 tienen pruebas en el host, no verificación en la placa. Ver `ROADMAP.md` |

Números históricos del banco: un turno con cuatro herramientas en 13.7 s; consola por TLS en 0.55 s;
escaneo I²C estable en 8 ejecuciones. La lectura antigua de 5–6 kB de SRAM interna libre tras una página TLS
es anterior a las correcciones del asignador (`10b16dd`) y no es un límite actual. Las mediciones posteriores
por pool y su procedencia están en [Solución de problemas](docs/troubleshooting.md#how-much-memory-the-board-has).
El firmware de la auditoría no se ha grabado ni probado en la placa.

## Documentación

[Arquitectura](docs/architecture.md) · [Manifiesto](docs/manifest.md) · [Capacidades](docs/capabilities.md) ·
[API HTTP y MCP](docs/http-api.md) · [Añadir un dispositivo](docs/adding-a-device.md) · [Grabación](docs/flashing.md) ·
[Solución de problemas](docs/troubleshooting.md) · [Bucle de agente](docs/agent-loop.md) · [Flujo](docs/agent-stream.md) ·
[Skills](docs/agent-skills.md) · [Memoria](docs/agent-memory.md) · [Scripting](docs/scripting.md) ·
[Model](docs/model.md) · [Web console](docs/web-console.md) · [Scheduling](docs/scheduling.md) · [Shell](docs/shell.md) · [Context layers](docs/context.md) · [Security](docs/security.md) · [Simulator](docs/simulator.md) · [Audio](docs/audio.md) · [Informe de auditoría](docs/audit-2026-11.md)

## Contribuir

Un periférico nuevo empieza como fragmento de manifiesto en `examples/manifests/`; un driver en
Rust solo se necesita para temporización, calibración o estado. `CATALOG` es donde un driver se
registra. Las convenciones están en [AGENTS.md](AGENTS.md) y [CONTRIBUTING.md](CONTRIBUTING.md).

## Licencia

MIT. Ver [LICENSE](LICENSE).
