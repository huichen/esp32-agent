# esp32-agent

**العربية** · [English](README.md) · [简体中文](README.zh.md) · [Français](README.fr.md) · [Español](README.es.md) · [Русский](README.ru.md)

برنامج ثابت لـ ESP32 يشغّل حلقة وكيل خاصة به ويقدّم نقطة نهاية MCP، فيستطيع نموذج واحد قيادة
اللوحة مباشرة، ويستطيع أي وكيل خارجي استخدام الأدوات نفسها عبر HTTP.

يُحرَق، يتصل بشبكة WiFi، ثم يسبر النواقل التي لديه مشغلات لها. كل جهاز يستجيب يصبح أدوات MCP.
الوكيل على اللوحة يرى الأدوات نفسها ويجيب بها؛ ويمكن الإعلان عن حسّاس جديد بصيغة JSON واللوحة
تعمل.

> حلقة الوكيل تتبع [FutureOS](https://github.com/futuregene/future-os): سجل المحادثة، سجل
> استدعاءات الأدوات، أحداث التدفق، مهايئات البروتوكول، فهرس الذاكرة. وحيث يختلف البرنامج الثابت
> (كيلوبايتات بدل غيغابايتات من RAM، ولا ملخصات يكتبها النموذج) يشرح الكود السبب.

| ما | بالتفصيل |
|---|---|
| نموذج يعمل على اللوحة | الحلقة والأدوات والذاكرة وسجل المحادثة كلها في البرنامج الثابت؛ والنموذج أي نقطة نهاية متوافقة مع OpenAI أو مع Anthropic. [كيف تعطيه نموذجا](docs/model.md) |
| مجموعة الأدوات مستند | `manifest_apply` يتحقق من بيان JSON ويحوّله إلى أدوات سارية في الدورة نفسها. [البيان](docs/manifest.md) |
| يجد عتاده بنفسه | يُسبر I2C و1-Wire والأطراف المعلنة عند الإقلاع؛ ولا يحصل على أدوات إلا ما يستجيب. [القدرات](docs/capabilities.md) |
| مجموعة أدوات واحدة، مستدعيان | نقطة نهاية MCP والوكيل على اللوحة يريان الأدوات نفسها من وصف واحد. [HTTP وMCP API](docs/http-api.md) |
| وحدة تحكم عبر HTTPS | محادثة مع النموذج وإعداد للأجهزة والأدوات والجدولة والسجلات وWiFi والنموذج من هاتف؛ واللوحة تصدر شهادتها بنفسها. [وحدة تحكم الويب](docs/web-console.md) |
| يعمل بلا مراقبة | موجّهات موقوتة وقواعد حسّاسات تُطلق موجّها عندما تتجاوز قراءة عتبة ما. [العمل المجدول](docs/scheduling.md) |
| طرفية ولغة سكربت | `shell` بـ 26 أمرا بأسماء من NuttX، ومفسّر لحلقات على استدعاءات الأدوات. [Shell](docs/shell.md) · [Script](docs/scripting.md) |
| يتذكر ما قيل له | أوامر دائمة، و`/AGENTS.md`، وفهرس `/memory.md`، ومهارات، تُجمَع في الموجّه في كل دورة. [ما يراه النموذج دائما](docs/context.md) |
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

## وحدة تحكم الويب

| أين | ما تفعله هناك |
|---|---|
| المحادثة (الصفحة) | التحدث إلى النموذج؛ استدعاءات الأدوات والتفكير تُطوى في سطر واحد لكل دورة |
| الأجهزة | ما وجده السبر، الأطراف، العناوين، `Probe devices`، مرشحات قابلة للتبنّي |
| الأدوات | كل أداة مع مخطط مدخلاتها، وسبب إخفاء واحدة |
| الوكيل | نقطة النهاية، النموذج، المفتاح، البروتوكول، مستوى التفكير، الحرارة، حجم السياق والإخراج |
| الجدولة | موجّهات موقوتة وقواعد حسّاسات |
| السجلات، الإعدادات | المخزن الدائري؛ WiFi مع تجاوزات البوابة وDNS، ورقم PIN، وإعادة التشغيل |

لا خطوة بناء ولا شيء يُحمَّل من الإنترنت: نحو 58 KB من HTML وCSS وJavaScript مترجمة داخل
البرنامج الثابت وتُقدَّم مع `Cache-Control: no-store`، فإعادة الحرق تظهر عند التحميل التالي.

## كيف تعطيه نموذجا

```bash
curl -sS -X POST http://<board>/api/agent/config -H 'Content-Type: application/json' \
  -d '{"base_url":"https://future-os.cn/api/v1/chat/completions",
       "model":"deepseek-flash","api_key":"<key>"}'
curl -sS -X POST http://<board>/api/agent/message -H 'Content-Type: application/json' \
  -d '{"text":"write HELLO on the OLED, set the LED blue, then scan I2C"}'
```

ثلاثة بروتوكولات: `chat` (اكتمالات محادثة OpenAI)، و`responses` (واجهة Responses من OpenAI)،
و`anthropic` (واجهة Messages). الصور من وحدة التحكم تخرج كأجزاء محتوى. `reasoning` يضبط مستوى
التفكير، و`max_steps` يحدّد سقف استدعاءات النموذج في الدورة (0 = بلا سقف)، و`context_tokens`
و`max_output_tokens` يضبطان النافذة وحجز الرد، ويُضغط سجل المحادثة عند 80% من النافذة. بلا مفتاح،
وجّه `base_url` إلى `/selftest/chat/completions` على اللوحة نفسها، وتبقى الحلقة تعمل.

## ما يراه النموذج دائما

أربع طبقات تدخل موجّه النظام، بهذا الترتيب، في **كل** دورة: ما كتبه المالك مرة واحدة، وما تعلّمه الوكيل بعدها. لا شيء منها في سجل المحادثة، فتكلفتها واحدة سواء كانت المحادثة رسالة واحدة أو ألفا.

| الطبقة | أين تعيش | كم يصل إلى النموذج | من يكتبها |
|---|---|---|---|
| أوامر دائمة | `agent.system_prompt` (NVS) | كلها، بسقف 2 KB | المالك، عبر HTTP أو `shell` |
| تعليمات المشروع | `/AGENTS.md` | أول 1 KB، ثم علامة تقول إن الباقي هناك | المالك، عبر `fs_write` |
| ذاكرة طويلة المدى | `/memory.md`، فهرس يشير إلى `/memory/<topic>.md` | الفهرس وحده، 24 سطرا / 2 KB | الوكيل نفسه، مع ما يتعلمه |
| المهارات | `/skills/<name>/SKILL.md` وما يجاوره | سطر لكل واحدة، الاسم والوصف | أي منهما |

```bash
# أوامر المالك الدائمة: لماذا توجد هذه اللوحة، وما القواعد التي تأتي أولا.
curl -sS -X POST http://<board>/api/agent/prompt -H 'Content-Type: application/json' \
  -d '{"action":"set","text":"مرحّل المضخة على GPIO12. لا تشغّله أكثر من دقيقتين."}'

# ما سيستلمه النموذج فعلا، بكل الطبقات — الطريقة الوحيدة للتحقق من ذلك.
curl -sS http://<board>/api/agent/prompt | jq -r .system
```

`action` هو `set` أو `append` أو `clear`؛ و`system_prompt get` يقرأه عائدا من اللوحة، و
`system_prompt set …` يكتبه من جهة الوكيل. الذاكرة والمهارات ملفات عادية:
`fs_write /memory/wiring.md "GPIO18 = شريط LED"` مع سطر واحد في `/memory.md` هو كل الذاكرة،
وحذف الملف ينسيها. الملف المعدَّل يسري في الدورة التالية: بلا إعادة تشغيل، ولا إعادة حرق، ولا
إعادة كتابة للسجل.

الأحجام هي سبب هذا التقسيم. لوحة بكيلوبايتات من SRAM الداخلية لا تستطيع حمل كل ما تعرفه في كل
طلب، فكل طبقة فهرس أو بادئة، والجزء المكلف يُجلب بـ `fs_read` عندما تحتاجه دورة فعلا.
[docs/agent-memory.md](docs/agent-memory.md) و[docs/agent-skills.md](docs/agent-skills.md) فيهما
التفاصيل، بما في ذلك كيف يُحدّ الفهرس.

## Shell

كل مشغّل في الكتالوج موثّق في [docs/capabilities.md](docs/capabilities.md): GPIO، ADC، PWM، I2C،
1-Wire، WS2812، SSD1306، BME280، SHT3x، INA219/226، MPU6050، ADS1115، PCF8574، DS18B20،
HC-SR04، سيرفو، مرحّل، ميكروفون ومكبّر I²S.

منطقة واحدة قابلة للكتابة، 8 KB من النص في NVS. `read` و`write` و`edit` تغطي الحالات الشائعة؛
وكل ما عداها سطر أوامر `shell` واحد، يُحلَّل إلى كلمات وعلامات اقتباس و`--flags`. نتيجة الأداة
محدودة بـ 2 KB والرد بـ 8 KB: للوحة 8 MB من PSRAM وبضعة كيلوبايتات من SRAM الداخلية، والرقم
الثاني هو المُلزِم:

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

`edit` يرفض تطابقا غامضا بدل إعادة كتابة الموضع الخطأ؛ و`rm` و`mv` يحتاجان `confirm`. الأمر
الخاطئ يجيب بسطر استخدامه، مأخوذا من الجدول نفسه الذي يأتي منه نص المساعدة، فلا يمكن أن يختلفا:
`rm` بلا `--confirm` يعيد `usage: rm <path> --confirm`. و`shell {"command":"help"}` يسرد الـ26.

## Script

لما لا تستطيع الأدوات الثابتة التعبير عنه: حلقة على قراءات، أو تفريع على نتيجة.

```text
let scan = tool("i2c_scan", {})
let i = 0
while i < len(scan.addresses) { log(scan.addresses[i])  i = i + 1 }
if i > 0 { "found " + str(i) } else { "nothing" }
```

أرقام، سلاسل، قيم منطقية، nil، قوائم، خرائط؛ `let`، إسناد، `if`/`else`، `while`، `break`؛ دوال
مدمجة `len`، `str`، `num`، `type`، `log` و`tool(name, args)` التي تستدعي أي أداة على اللوحة
بأذونات المستدعي نفسه (عدا `tool("script")`). `{"lint":true}` يفحص الصياغة والأسماء المجهولة مع
السطر والعمود ولا ينفّذ أبدا؛ `{"help":true}` يعيد المرجع. الحدود: 30 s، خمسة ملايين خطوة، 32
كتلة متداخلة، سلاسل 4 KB، وملف مصدري 8 KB. الساعة هي الحد الحقيقي: المفسّر يعمل في الخيط الذي
يجيب وحدة التحكم.

## العمل المجدول

```bash
curl -sS -X POST http://<board>/api/sched -H 'Content-Type: application/json' -d '{
  "tasks":[{"id":"t1","prompt":"read the BME280","every_s":300}],
  "rules":[{"id":"r1","tool":"bme280_read","path":"temperature_c","op":"gt","value":30,"prompt":"it is hot"}]}'
```

`at_hhmm` يشغّل المهمة عند وقت ساعة UTC بدلا من ذلك، وكل جواب يصل إلى سجل المحادثة موسوما
`source: "schedule:t1"`. والوكيل يستطيع جدولة نفسه بـ `schedule {"action":"add", …}`. المهام
تصمد بعد إعادة التشغيل.

## البيان

الجهاز الذي لا يحتاج توقيتا ولا معايرة يُعلَن ولا يُترجَم:

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

ثلاثة مسارات تضيف عتادا: بيان (بلا توقيت ولا آلة حالات، يسري فورا)، أو مشغّل مترجم
(`drivers/<chip>.rs`، لآلات الحالات والتوقيت الصارم)، أو ملف تعريف للوحة (حقائق لا يمكن سبرها،
مثل أي أطراف يستخدمها المصباح والأزرار). المشغّل لا يُدرج إلا عندما يجد السبر الجهاز، فمشغّل نصف
مكتوب لا يستطيع الإبلاغ عن عتاد غير موجود. التحقق يجري على اللوحة: نطاقات المعاملات وسقف
`safety` في الدقيقة يُفحصان قبل أن يتحرك أي طرف، والاستدعاء خارج النطاق يعيد النطاق الذي
سيقبله. [docs/adding-a-device.md](docs/adding-a-device.md) يغطي الثلاثة.

## المصادقة

| المستدعي | بيانات الاعتماد |
|---|---|
| وحدة التحكم | `POST /api/auth {"pin":"…"}` يعيد رمز bearer تحتفظ به الصفحة |
| Script | `X-Portal-Pin: <pin>`، أو الرمز نفسه |
| عميل MCP | `Authorization: Bearer <mcp.token>`؛ ويُقبل PIN عندما لا يكون هناك رمز |

مع ضبط `auth.portal_pin` تحتاجه الكتابات والقراءات التي تكشف سجل المحادثة والموجّه والإعدادات
والسجلات؛ وتبقى الحالة والأجهزة والأدوات و`/agent.json` مفتوحة. الرموز تعيش في RAM، فإعادة
التشغيل تبطلها. الوضع الافتراضي بلا PIN، و`/agent.json` يقول ذلك في كتلة `security`.

## المحاكي

```bash
cargo run -p esp32-agent-sim
```

`agent-core` نفسه على لوحة افتراضية على منفذ حقيقي. لا يختلف إلا تنفيذ `Hardware`، فوحدة التحكم
ونقطة نهاية MCP والمشغلات ومحرك البيان وحلقة الوكيل هي الكود الذي يعمل على الشريحة.

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
[المهارات](docs/agent-skills.md) · [الذاكرة](docs/agent-memory.md) · [Scripting](docs/scripting.md)

## المساهمة

الطرفية الجديدة تبدأ كجزء بيان في `examples/manifests/`؛ ولا يُحتاج إلى مشغّل Rust إلا للتوقيت أو
المعايرة أو الحالة. `CATALOG` هو المكان الذي يسجّل فيه المشغّل نفسه. الأعراف في
[AGENTS.md](AGENTS.md) و[CONTRIBUTING.md](CONTRIBUTING.md).

## الترخيص

MIT. انظر [LICENSE](LICENSE).
