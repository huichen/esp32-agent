# esp32-agent

**Español** · [English](README.md) · [简体中文](README.zh.md) · [Français](README.fr.md) · [Русский](README.ru.md) · [العربية](README.ar.md)

Firmware para un ESP32 que ejecuta su propio bucle de agente y sirve un endpoint MCP, de modo
que un modelo puede gobernar la placa directamente y cualquier agente externo puede usar las
mismas herramientas por HTTP.

Se graba, se conecta a la WiFi y sondea los buses para los que tiene drivers. Cada dispositivo
que responde se convierte en herramientas MCP. El agente de la placa ve esas mismas
herramientas y responde con ellas; se puede declarar un sensor nuevo en JSON con la placa en
marcha.

> El bucle de agente sigue a [FutureOS](https://github.com/futuregene/future-os): transcripción,
> diario de herramientas, eventos de flujo, adaptadores de protocolo, índice de memoria. Donde
> el firmware se aparta (kilobytes en lugar de gigabytes de RAM, sin resúmenes escritos por el
> modelo) el código dice por qué.

| Qué | En detalle |
|---|---|
| Un modelo se ejecuta en la placa | El bucle, las herramientas, la memoria y la transcripción son firmware; el modelo es cualquier endpoint compatible con OpenAI o de Anthropic. [Cómo darle un modelo](#cómo-darle-un-modelo) |
| El conjunto de herramientas es un documento | `manifest_apply` valida un manifiesto JSON, lo compila en herramientas y está activo en el mismo turno. [Manifiesto](#manifiesto) |
| Encuentra su propio hardware | I2C, 1-Wire y los pines declarados se sondean al arrancar; solo lo que responde obtiene herramientas. [Capacidades](docs/capabilities.md) |
| Un conjunto de herramientas, dos invocadores | El endpoint MCP y el agente de la placa ven las mismas herramientas, desde una sola descripción. [API HTTP y MCP](docs/http-api.md) |
| Una consola sobre HTTPS | Hablar con el modelo y configurar dispositivos, herramientas, programación, registros, WiFi y el modelo desde un teléfono; la placa emite su propio certificado. [Consola web](#consola-web) |
| Actúa sin nadie delante | Prompts temporizados y reglas de sensores que lanzan uno al cruzar un umbral. [Trabajo programado](#trabajo-programado) |
| Un shell y un lenguaje de script | `shell` con 26 comandos con nombres de NuttX, y un intérprete para bucles sobre llamadas a herramientas. [Shell](#shell) · [Script](#script) |
| Órdenes permanentes | 2 KB en el prompt de sistema para la finalidad de esta placa, escribibles desde la consola, `shell` y HTTP. [Órdenes permanentes](#órdenes-permanentes) |
| Sin nube en el camino | HTTPS desde la placa, un certificado que genera ella misma, un PIN para tres invocadores. [Autenticación](#autenticación) |
| Comprobable sin hardware | El simulador ejecuta el mismo `agent-core` contra una placa virtual. [Simulador](#simulador) |

## Grabación y conexión

```bash
cargo install espup espflash ldproxy
espup install
powershell scripts/build-firmware.ps1 -Flash -Port COM3
```

Sin credenciales guardadas, la placa levanta su propio punto de acceso. Se puede conectar a él
desde un teléfono y abrir `http://192.168.4.1/`, o enviar `provision <ssid> <password>` por USB.
Las credenciales van a NVS, nunca dentro de la imagen.

```bash
curl -sS http://<board>/agent.json | jq '.endpoints | length'
curl -sS -X POST http://<board>/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq '.result.tools | length'
curl -k -sS https://<board>/api/tls | jq -r .fingerprint
```

El puerto 443 sirve las mismas rutas sobre TLS con un certificado generado en el primer
arranque; los navegadores avisan, así que conviene comparar la huella. El puerto 80 sigue
abierto, así que un certificado malo cuesta una URL y no la placa. `GET /agent.json` es toda la
interfaz como datos: 40 rutas con método, cuerpo, retorno y autenticación, más cada ajuste
escribible con su tipo y su valor por defecto.

## Consola web

| Dónde | Qué se hace ahí |
|---|---|
| Chat (la página) | hablar con el modelo; las llamadas a herramientas y el razonamiento se pliegan en una línea por turno |
| Devices | lo que encontró el sondeo, pines, direcciones, `Probe devices`, candidatos adoptables |
| Tools | cada herramienta con su esquema de entrada, y por qué una está oculta |
| Agent | endpoint, modelo, clave, protocolo, nivel de razonamiento, temperatura, tamaño de contexto y de salida |
| Schedule | prompts temporizados y reglas de sensores |
| Logs, Settings | el búfer circular; WiFi con overrides de gateway y DNS, el PIN, reinicio |

Sin paso de compilación y sin cargar nada de internet: unos 58 KB de HTML, CSS y JavaScript
compilados dentro y servidos con `Cache-Control: no-store`, así que una regrabación se ve en la
siguiente recarga.

## Cómo darle un modelo

```bash
curl -sS -X POST http://<board>/api/agent/config -H 'Content-Type: application/json' \
  -d '{"base_url":"https://future-os.cn/api/v1/chat/completions",
       "model":"deepseek-flash","api_key":"<key>"}'
curl -sS -X POST http://<board>/api/agent/message -H 'Content-Type: application/json' \
  -d '{"text":"write HELLO on the OLED, set the LED blue, then scan I2C"}'
```

Tres protocolos: `chat`, `responses`, `anthropic`. Las imágenes de la consola salen como partes
de contenido. `reasoning` fija el nivel de razonamiento, `max_steps` limita las llamadas al
modelo por turno (0 = sin límite), `context_tokens` y `max_output_tokens` fijan la ventana y la
reserva de respuesta, y la transcripción se compacta al 80% de la ventana. Sin clave, se puede
apuntar `base_url` al `/selftest/chat/completions` de la propia placa y el bucle sigue
funcionando.

## Órdenes permanentes

Un espacio para el *para qué* de esta placa, escrito por su propietario y leído en cada turno:

```bash
curl -sS -X POST http://<board>/api/agent/prompt -H 'Content-Type: application/json' \
  -d '{"action":"set","text":"The pump relay is on GPIO12. Never run it past two minutes."}'
```

`action` es `set`, `append` o `clear`; `GET /api/agent/prompt` muestra el prompt que recibirá el
modelo, incluida la parte personalizada, y el agente llega a él con
`shell {"command":"system_prompt set …"}`. 2 KB en NVS, así que sobrevive a un reinicio.

## Shell

Todos los drivers del catálogo están documentados en [docs/capabilities.md](docs/capabilities.md):
GPIO, ADC, PWM, I2C, 1-Wire, WS2812, SSD1306, BME280, SHT3x, INA219/226, MPU6050, ADS1115,
PCF8574, DS18B20, HC-SR04, servo, relé, micrófono y amplificador I²S.

Una sola zona escribible, 8 KB de texto en NVS. `read`, `write` y `edit` cubren los casos
habituales; todo lo demás es una línea de comandos `shell`, analizada con palabras, comillas y
`--flags`. Un resultado de herramienta está limitado a 2 KB y una respuesta a 8 KB: la placa
tiene 8 MB de PSRAM y unos pocos kilobytes de SRAM interna, y el segundo número es el que
manda:

```text
shell {"command":"ls /notes"}            shell {"command":"grep GPIO /"}
shell {"command":"cp /a.txt /b.txt"}     shell {"command":"rm /a.txt --confirm"}
shell {"command":"curl https://example.com/"}          # GET, HTML reduced to text
shell {"command":"wget -o /dl/page.txt https://example.com/"}
shell {"command":"ping -c 3 192.168.3.1"}   shell {"command":"nslookup example.com"}
shell {"command":"ifconfig"}                # configured gateway and DNS, not the lease
shell {"command":"df | free | uptime | date | uname | ps"}
shell {"command":"sha256sum /notes/a.txt"}  shell {"command":"zip t.zip /a.txt"}
```

`edit` rechaza una coincidencia ambigua en lugar de reescribir el lugar equivocado; `rm` y `mv`
necesitan `confirm`. Un comando mal escrito responde con su línea de uso, tomada de la misma
tabla de la que sale el texto de ayuda, así que las dos no pueden discrepar: `rm sin --confirm`
devuelve `usage: rm <path> --confirm`. `shell {"command":"help"}` lista los 26.

## Script

Para lo que las herramientas fijas no pueden expresar: un bucle sobre lecturas, una rama sobre
un resultado.

```text
let scan = tool("i2c_scan", {})
let i = 0
while i < len(scan.addresses) { log(scan.addresses[i])  i = i + 1 }
if i > 0 { "found " + str(i) } else { "nothing" }
```

Números, cadenas, booleanos, nil, listas, mapas; `let`, asignación, `if`/`else`, `while`,
`break`; funciones integradas `len`, `str`, `num`, `type`, `log` y `tool(name, args)`, que llama
a cualquier herramienta de la placa con los permisos del propio invocador (salvo
`tool("script")`). `{"lint":true}` comprueba la sintaxis y los nombres desconocidos con línea y
columna y nunca ejecuta; `{"help":true}` devuelve la referencia. Límites: 30 s, cinco millones
de pasos, 32 bloques anidados, cadenas de 4 KB, 8 KB de código fuente. El reloj es el límite
real: el intérprete corre en el hilo que atiende la consola.

## Trabajo programado

```bash
curl -sS -X POST http://<board>/api/sched -H 'Content-Type: application/json' -d '{
  "tasks":[{"id":"t1","prompt":"read the BME280","every_s":300}],
  "rules":[{"id":"r1","tool":"bme280_read","path":"temperature_c","op":"gt","value":30,"prompt":"it is hot"}]}'
```

`at_hhmm` ejecuta a una hora de reloj UTC en su lugar. El agente puede programarse a sí mismo
con `schedule {"action":"add", …}`. Las tareas sobreviven a un reinicio.

## Manifiesto

Un dispositivo que no necesita temporización ni calibración se declara, no se compila:

```json
{"devices":[{"id":"pump","kind":"actuator","pins":{"out":12},"note":"active-low relay"}],
 "tools":[{"name":"pump_pulse","description":"Energise the relay for `ms` ms",
   "params":[{"name":"ms","type":"integer","min":10,"max":5000,"required":true}],
   "exec":{"type":"sequence","steps":[
     {"ms":0,"exec":{"type":"gpio_write","pin":12,"level":0}},
     {"ms":0,"exec":{"type":"delay","ms":{"param":"ms"}}},
     {"ms":0,"exec":{"type":"gpio_write","pin":12,"level":1}}]},
   "safety":{"max_calls_per_min":20,"confirm":true}}]}
```

Tres caminos añaden hardware: un manifiesto (sin temporización ni máquina de estados, se aplica
en caliente), un driver compilado (`drivers/<chip>.rs`, para máquinas de estados y temporización
estricta), o un perfil de placa (hechos que no se pueden sondear, como qué pines usan el LED y
los botones). Un driver solo se lista cuando el sondeo encuentra el dispositivo, así que uno a
medio escribir no puede informar de hardware que no está.
[docs/adding-a-device.md](docs/adding-a-device.md) cubre ambos.

## Autenticación

| Invocador | Credencial |
|---|---|
| Consola | `POST /api/auth {"pin":"…"}` devuelve un token bearer que la página guarda |
| Script | `X-Portal-Pin: <pin>`, o el mismo token |
| Cliente MCP | `Authorization: Bearer <mcp.token>`; se acepta el PIN cuando no hay token configurado |

Con `auth.portal_pin` configurado, lo necesitan las escrituras y las lecturas que exponen la
transcripción, el prompt, la configuración y los registros; estado, dispositivos, herramientas
y `/agent.json` siguen abiertos. Los tokens viven en RAM, así que un reinicio los revoca. Por
defecto no hay PIN, y `/agent.json` lo dice en su bloque `security`.

## Simulador

```bash
cargo run -p esp32-agent-sim
```

El mismo `agent-core` contra una placa virtual en un puerto real. Solo cambia la implementación
de `Hardware`, así que la consola, el endpoint MCP, los drivers, el motor de manifiestos y el
bucle de agente son el código que corre en el chip.

## Estructura del repositorio

`crates/` compila y se prueba en un host; `firmware/` es el backend de ESP-IDF. `agent-core` es
la placa detrás de un único trait `Hardware` sin código específico de chip, así que `cargo test`
lo ejecuta en un portátil; `agent-sim` implementa ese trait sobre un fichero de registros.
Fusionarlos metería el simulador en cada imagen y las dependencias del firmware en cada prueba.
`firmware/` queda fuera del workspace porque necesita `cargo +esp`, `-Zbuild-std` y un ESP-IDF
descargado.

```
crates/agent-core/    hardware.rs (the trait) · drivers/ (one module per chip) ·
                      manifest.rs · agent.rs + llm.rs · sched.rs · tools/ ·
                      http.rs · app.rs · sitemap.rs + settings.rs · virtual_board.rs
crates/agent-sim/     the virtual board behind an HTTP port
firmware/             main.rs (boot order) · runner.rs (agent thread, TLS) ·
                      esp_hw.rs (GPIO/I2C/PWM/ADC/RMT/I2S) · console.rs · wifi.rs
```

## Estado

| Parte | Estado |
|---|---|
| `agent-core` | 424 pruebas unitarias y 14 de integración pasan; `scripts/smoke.ps1` pasa de extremo a extremo |
| Simulador | la consola, MCP y el bucle de agente funcionan de extremo a extremo |
| Firmware | grabado en una ESP32-S3-DevKitC-1 N16R8 (IDF v5.5.2): 42 herramientas, OLED 128×32, micrófono y amplificador I²S, WS2812, consola TLS, turnos del modelo por HTTPS |
| No implementado | OTA, tarjeta SD, DHT11/22, cámara. Ver `ROADMAP.md` |

Números del banco: un turno con cuatro herramientas en 13.7 s; consola por TLS en 0.55 s;
escaneo I²C estable en 8 ejecuciones; SRAM interna 5–6 kB libres tras una página TLS, la
restricción más ajustada aquí.

## Documentación

[Arquitectura](docs/architecture.md) · [Manifiesto](docs/manifest.md) · [Capacidades](docs/capabilities.md) ·
[API HTTP y MCP](docs/http-api.md) · [Añadir un dispositivo](docs/adding-a-device.md) · [Grabación](docs/flashing.md) ·
[Solución de problemas](docs/troubleshooting.md) · [Bucle de agente](docs/agent-loop.md) · [Flujo](docs/agent-stream.md) ·
[Skills](docs/agent-skills.md) · [Memoria](docs/agent-memory.md) · [Scripting](docs/scripting.md)

## Contribuir

Un periférico nuevo empieza como fragmento de manifiesto en `examples/manifests/`; un driver en
Rust solo se necesita para temporización, calibración o estado. `CATALOG` es donde un driver se
registra. Las convenciones están en [AGENTS.md](AGENTS.md) y [CONTRIBUTING.md](CONTRIBUTING.md).

## Licencia

MIT. Ver [LICENSE](LICENSE).
