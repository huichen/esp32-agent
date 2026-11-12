# esp32-agent

**العربية** · [English](README.md) · [简体中文](README.zh.md) · [Français](README.fr.md) · [Español](README.es.md) · [Русский](README.ru.md)

برنامج ثابت لـ ESP32 يشغّل حلقة وكيل خاصة به ويقدّم نقطة نهاية MCP، فيستطيع نموذج واحد قيادة
اللوحة مباشرة، ويستطيع أي وكيل خارجي استخدام الأدوات نفسها عبر HTTP.

يُحرَق، يتصل بشبكة WiFi، ثم يسبر النواقل التي لديه مشغلات لها. كل جهاز يستجيب يصبح أدوات MCP.
الوكيل على اللوحة يرى الأدوات نفسها ويجيب بها؛ ويمكن الإعلان عن حسّاس جديد بصيغة JSON واللوحة
تعمل.

> حلقة الوكيل تتبع [FutureOS](https://github.com/futuregene/future-os): سجل المحادثة، سجل
> استدعاءات الأدوات، أحداث التدفق، مهايئات البروتوكول، فهرس الذاكرة.

| ما | بالتفصيل |
|---|---|
| نموذج يعمل على اللوحة | الحلقة والأدوات والذاكرة وسجل المحادثة كلها في البرنامج الثابت؛ والنموذج أي نقطة نهاية متوافقة مع OpenAI أو مع Anthropic. [كيف تعطيه نموذجا](docs/model.md) |
| مجموعة الأدوات مستند | `manifest_apply` يتحقق من بيان JSON ويحوّله إلى أدوات سارية في الدورة نفسها. [البيان](docs/manifest.md) |
| يجد عتاده بنفسه | يُسبر I2C و1-Wire والأطراف المعلنة عند الإقلاع؛ ولا يحصل على أدوات إلا ما يستجيب. [القدرات](docs/capabilities.md) |
| مجموعة أدوات واحدة، مستدعيان | نقطة نهاية MCP والوكيل على اللوحة يريان الأدوات نفسها من وصف واحد. [HTTP وMCP API](docs/http-api.md) |
| وحدة تحكم عبر HTTPS | محادثة مع النموذج وإعداد للأجهزة والأدوات والجدولة والسجلات وWiFi والنموذج من هاتف؛ واللوحة تصدر شهادتها بنفسها. [وحدة تحكم الويب](docs/web-console.md) |
| يعمل بلا مراقبة | موجّهات موقوتة وقواعد حسّاسات تُطلق موجّها عندما تتجاوز قراءة عتبة ما. [العمل المجدول](docs/scheduling.md) |
| طرفية ولغة سكربت | `shell` بـ 26 أمرا بأسماء من NuttX، ومفسّر لحلقات على استدعاءات الأدوات. [Shell](docs/shell.md) · [Script](docs/scripting.md) |
| يتذكر ما قيل له | أوامر دائمة، و`/AGENTS.md`، وفهرس `/MEMORY.md`، ومهارات، تُجمَع في الموجّه في كل دورة. [ما يراه النموذج دائما](docs/context.md) |
| لا سحابة في الطريق | HTTPS من اللوحة، وشهادة تولّدها بنفسها، ورقم PIN واحد لثلاثة مستدعين. [المصادقة](docs/security.md) |
| قابل للاختبار بلا عتاد | المحاكي يشغّل `agent-core` نفسه على لوحة افتراضية. [المحاكي](docs/simulator.md) |

## التثبيت والاتصال

```bash
cargo install espup espflash ldproxy
espup install
powershell scripts/build-firmware.ps1 -Flash -Port COM3
```

بلا بيانات اعتماد مخزّنة ترفع اللوحة نقطة وصول خاصة بها. اتصل بها من هاتف وافتح
`http://192.168.4.1/`، أو أرسل `provision <ssid> <password>` عبر USB. تذهب بيانات الاعتماد إلى
NVS، ولا تدخل الصورة أبدا.

```bash
curl -sS http://<board>/agent.json | jq '.endpoints | length'
curl -sS -X POST http://<board>/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq '.result.tools | length'
curl -k -sS https://<board>/api/tls | jq -r .fingerprint
```

المنفذ 443 يقدّم المسارات نفسها عبر TLS بشهادة تُولَّد عند أول إقلاع؛ والمتصفحات تحذّر، لذا قارن
البصمة. المنفذ 80 يبقى مفتوحا، فشهادة سيئة تكلّف عنوان URL لا اللوحة. `GET /agent.json` هو
الواجهة كلها كبيانات: 40 مسارا بالطريقة والجسم والقيمة المرجعة والمصادقة، مع كل إعداد قابل
للكتابة ونوعه وقيمته الافتراضية.

## أعطه نموذجا

النموذج إعداد وليس بناء:

```bash
curl -sS -X POST http://<board>/api/agent/config -H 'Content-Type: application/json' \
  -d '{"base_url":"https://your-provider/v1/chat/completions","model":"…","api_key":"…"}'
curl -sS -X POST http://<board>/api/agent/message -H 'Content-Type: application/json' \
  -d '{"text":"اكتب HELLO على شاشة OLED واجعل المصباح أزرق"}'
```

ثلاثة بروتوكولات (`chat` و`responses` و`anthropic`)، ومستوى تفكير، ونافذة سياق، وسقف للمخرجات،
كلها في لوحة Agent داخل وحدة التحكم أو بنفس طلب `POST`. وبدون مفتاح، وجّه `base_url` إلى
`http://127.0.0.1/selftest/chat/completions` وسيعمل الحلقة كما هي.
[Model](docs/model.md) فيه جدول الحقول والبروتوكولات الثلاثة جنبا إلى جنب.

## الاستخدام

| من | ماذا تحصل |
|---|---|
| `http://<board>/` من الهاتف | محادثة مع النموذج، ورؤية الأجهزة والأدوات، وتغيير كل شيء |
| `POST /mcp` من عميل MCP | نفس الأدوات التي يستخدمها وكيل اللوحة |
| `POST /api/script` و`POST /api/sched` | شغّل سكربتا الآن، أو جدوله |
| `GET /agent.json` | الواجهة كلها كبيانات: المسارات والإعدادات والحدود |

```bash
curl -sS -X POST http://<board>/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"shell","arguments":{"command":"ls /"}}}'
```

ويستطيع وكيل اللوحة أن يضيف لنفسه عتادا (`manifest_apply`)، وذاكرة (`write /memory/…`)،
ووصولا إلى shell: نفس النداءات ونفس PIN.

## ملاحظات أمنية

- **تبقى بيانات الاعتماد على اللوحة.** شبكة WiFi تكتب في NVS عبر `provision` ولا تدخل الصورة؛
  و`api_key` للكتابة فقط ويُقرأ كـ`__set__`.
- **اضبط PIN قبل أن تخرج اللوحة من مكتبك.** `auth.portal_pin` يحمي كل عمليات الكتابة والقراءات
  التي تكشف النص والمطالبة والإعدادات والسجلات.
- **قارن الشهادة.** شهادة اللوحة موقعة ذاتيا، وبصمتها في `GET /api/tls`. والمنفذ 80 مفتوح افتراضيا: شهادة خاطئة تكلف رابطا لا اللوحة؛ و`mcp.plain_http: false` يغلقه، وحينها لا سبيل للعودة إلا بكبل USB.
- **`shell` و`script` وأدوات البيان تحرك الأطراف.** هذا هو الغرض من اللوحة؛ وPIN هو الفاصل بين
  وكيل يقوم بعمله ومارّ يقوم به.

[Security](docs/security.md) يوضح ما لا يُدافع عنه عن قصد.

## بنية المستودع

`crates/` يُبنى ويُختبر على مضيف؛ و`firmware/` هو خلفية ESP-IDF. `agent-core` هو اللوحة خلف سمة
`Hardware` واحدة بلا كود خاص بشريحة، فيشغّله `cargo test` على حاسوب محمول؛ و`agent-sim` ينفّذ
تلك السمة على ملف سجلات. دمجهما يضع المحاكي في كل صورة وتبعيات البرنامج الثابت في كل اختبار.
`firmware/` خارج الـworkspace لأنه يحتاج `cargo +esp` و`-Zbuild-std` وESP-IDF مسحوبا.

```
crates/agent-core/    hardware.rs (the trait) · drivers/ (one module per chip) · boards.rs ·
                      manifest.rs · agent.rs + llm.rs · sched.rs · tools/ ·
                      http.rs · app.rs · sitemap.rs + settings.rs · virtual_board.rs
crates/agent-sim/     the virtual board behind an HTTP port
firmware/             main.rs (boot order) · runner.rs (agent thread, TLS) ·
                      esp_hw.rs (GPIO/I2C/PWM/ADC/RMT/I2S) · console.rs · wifi.rs
```

## الحالة

| الجزء | الحالة |
|---|---|
| `agent-core` | 427 اختبار وحدة و14 اختبار تكامل تنجح؛ و`scripts/smoke.ps1` ينجح من البداية إلى النهاية |
| المحاكي | وحدة التحكم وMCP وحلقة الوكيل تعمل من البداية إلى النهاية |
| البرنامج الثابت | محروق على ESP32-S3-DevKitC-1 N16R8 (IDF v5.5.2): 42 أداة، OLED 128×32، ميكروفون ومكبّر I²S، WS2812، وحدة تحكم عبر TLS، دورات النموذج عبر HTTPS |
| غير منفَّذ | OTA، بطاقة SD، DHT11/22، كاميرا. انظر `ROADMAP.md` |

أرقام منصة الاختبار: دورة بأربع أدوات في 13.7 s؛ وحدة التحكم عبر TLS في 0.55 s؛ مسح I²C مستقر على 8
تشغيلات؛ SRAM الداخلية 5–6 kB حرة بعد صفحة TLS، وهو أضيق قيد هنا.

## الوثائق

[المعمارية](docs/architecture.md) · [البيان](docs/manifest.md) · [القدرات](docs/capabilities.md) ·
[HTTP وMCP](docs/http-api.md) · [إضافة جهاز](docs/adding-a-device.md) · [الحرق](docs/flashing.md) ·
[استكشاف الأخطاء](docs/troubleshooting.md) · [حلقة الوكيل](docs/agent-loop.md) · [التدفق](docs/agent-stream.md) ·
[المهارات](docs/agent-skills.md) · [الذاكرة](docs/agent-memory.md) · [Scripting](docs/scripting.md) ·
[Model](docs/model.md) · [Web console](docs/web-console.md) · [Scheduling](docs/scheduling.md) · [Shell](docs/shell.md) · [Context layers](docs/context.md) · [Security](docs/security.md) · [Simulator](docs/simulator.md) · [Performance](docs/performance.md)

## المساهمة

الطرفية الجديدة تبدأ كجزء بيان في `examples/manifests/`؛ ولا يُحتاج إلى مشغّل Rust إلا للتوقيت أو
المعايرة أو الحالة. `CATALOG` هو المكان الذي يسجّل فيه المشغّل نفسه. الأعراف في
[AGENTS.md](AGENTS.md) و[CONTRIBUTING.md](CONTRIBUTING.md).

## الترخيص

MIT. انظر [LICENSE](LICENSE).
