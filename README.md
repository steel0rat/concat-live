# Concat Live

Форк [Concat 0.2.10](https://github.com/jub0t/Concat/tree/v0.2.10) с совместным
редактированием видимого проекта и нативной панелью Codex. Агент и пользователь
работают с одной сессией, таймлайном и историей отмены. Экспорт и предпросмотр
получают текущее состояние, включая несохранённые изменения.

На настольной системе обычный запуск, `--project DIR` и `--live` включают
live-режим. `--mcp window` подключается к нему; `--mcp headless` остаётся
отдельным процессом. На Android и iOS сохранён прежний редактор, панель
Codex и явный `--live` там не поддерживаются. Новые функции проверяются
на macOS Apple Silicon; Windows и Linux требуют отдельной проверки.

Пакеты форка для Apple Silicon и Intel публикуются в
[GitHub Releases](https://github.com/steel0rat/concat-live/releases).
Пакеты CI требуют macOS 26 или новее: эта версия нужна библиотекам кодеков,
которые входят в сборку. Пакеты без Developer ID подписываются ad-hoc
и не проходят нотаризацию Apple.

Для сборки нужны закреплённый Rust 1.93.1, CMake, компилятор C++ и библиотеки
разработки FFmpeg 7+ (общие инструкции ниже). Для сборки на macOS также нужны
Swift 6.2+ и SDK macOS 26+ из Xcode или Command Line Tools. Из корня репозитория:

```sh
cd src
cargo run --profile quick -p concat -- --live
```

Чтобы сразу открыть существующий проект, добавьте `--project /путь/к/проекту`.
Проект — каталог, содержащий `concat.json`. Для готового macOS-пакета
используется отдельное имя `Concat Live.app` и идентификатор
`ru.steel0rat.concat-live`. Его настройки и Automation discovery хранятся
отдельно от штатного Concat; данные исходного приложения не мигрируют.

## Чат Codex

Нужен установленный и авторизованный Codex CLI. Панель доступна справа
на главной странице и в редакторе. До создания проекта выберите рабочую
папку кнопкой Choose folder. В запросе укажите название проекта, размер
сцены, частоту кадров и фон, затем нажмите Send. Например:
«Создай проект Проба: 1080×1920, 30 кадров/с, белый фон».

Агент создаёт проект в выбранной папке, открывает его в окне и продолжает
монтаж в той же беседе. История и набранный черновик сообщения сохраняются
при переходе с главной в редактор. **History** открывает список сохранённых
бесед Concat с поиском по названию и рабочей папке. Выберите беседу, чтобы
продолжить её, или нажмите **Earlier messages**, чтобы пролистать её дальше
назад. При переключении у каждой беседы остаётся свой черновик до закрытия
приложения. Агент работает с проектом, который виден в редакторе.
**New chat** начинает отдельную беседу. **Reconnect Codex** восстанавливает
подключение к текущей беседе.

Обычные действия показываются краткими статусами. Аргументы инструментов,
их JSON-результаты и вывод команд в переписку не копируются. Сообщения
об ошибках остаются видимыми. Запросы на команды, правки файлов и разрешения
сохраняют подробности для проверки и требуют Allow once или Deny.
Interrupt останавливает текущий ход. Переключение уже открытого проекта
прерывает выполняемую операцию и сохраняет историю беседы.

Списки **Model** и **Reasoning** над полем ввода показывают модели и уровни
рассуждений, доступные у настроенного провайдера Codex. При выборе другой
модели подставляется её уровень по умолчанию. Выбор применяется со следующего
сообщения в той же беседе. Новая беседа начинается с настроек Codex CLI.

Чат использует локальный `codex app-server` и действующую конфигурацию CLI.
Ему нужен доступ к провайдеру Codex, который обрабатывает сообщения и
результаты инструментов. Медиаобработка и экспорт выполняются локально.
Подписи панели пока английские.

На macOS 26 и новее кнопка микрофона включает локальную диктовку Apple
DictationTranscriber. Выберите русский или английский язык и нажмите микрофон.
При первом использовании macOS запросит доступ к нему. Текущий текст
распознавания показывается отдельно от черновика; Stop завершает запись
и добавляет результат к набранному тексту. Затем сообщение можно отредактировать
и отправить кнопкой Send. Cancel отменяет диктовку и сохраняет черновик.

Аудио обрабатывается в памяти на этом Mac. Если языковая модель не установлена,
системе нужен интернет для её загрузки. Запись ограничена тремя минутами;
её можно начать и во время работы Codex, чтобы подготовить следующий запрос.
Во время диктовки отправка сообщения и смена беседы недоступны.
Закрытие окна отменяет диктовку. На других ОС и более ранних версиях macOS
кнопка недоступна.

Live Automation слушает случайный порт на `127.0.0.1`. Включение при запуске
не записывается в сохранённые пользовательские настройки. Отключение
Automation прекращает доступ агента к окну. Запросы к другому проекту,
переключение с несохранённой работы и конфликт с текущим жестом
возвращают ошибку; тихого перехода в headless нет. Закрытие проекта через
агента требует `save: true`; `save: false` в live-режиме всегда возвращает
отказ. Изменяющая команда
после потери ответа не повторяется автоматически: сначала нужно прочитать
фактическое состояние проекта.

## Навык по умолчанию и обновления

Перед каждым запросом панель перечитывает каталог навыков Codex и явно
подключает `steel0rat-video-maker-skill`. Устанавливать или вызывать его
по имени в сообщении не требуется. Если навыка нет или он отключён, запрос
не передаётся модели, а текст возвращается в черновик. Беседа сохраняется.

На macOS live-окно проверяет опубликованные стабильные релизы при запуске
и каждые шесть часов. Приложение берётся из
[steel0rat/concat-live](https://github.com/steel0rat/concat-live/releases),
навык — из
[steel0rat/steel0rat-video-maker-skill](https://github.com/steel0rat/steel0rat-video-maker-skill/releases).
Кнопка проверки на странице версии проверяет оба канала. Черновики релизов
и предварительные версии пропускаются. До первой публикации используется
уже установленный навык.

Навык хранится в каталогах версий в данных Concat Live. После проверки
SHA-256 и содержимого архива приложение переключает установленную ссылку
`~/.agents/skills/steel0rat-video-maker-skill`; прежняя ссылка или каталог
сохраняются отдельно. Рабочая папка, на которую указывала старая ссылка,
не изменяется. Новая версия подключается со следующего запроса той же беседы.

Пакет приложения проверяется до замены: идентификатор
`ru.steel0rat.concat-live`, версия, подпись и запуск `--smoke`.
Установка ждёт завершения работы агента, диктовки, воспроизведения и экспорта,
а также отправки или очистки черновика чата. Перед заменой проект сохраняется.
Прежняя сборка остаётся рядом с приложением; затем открывается тот же проект.
Ошибка сохранения оставляет редактор открытым. Переписка панели и её черновик
между запусками приложения пока не восстанавливаются.

Текущий автоматический выпуск предназначен для macOS Apple silicon и Intel.
Для локальной сборки вне `.app` автоматическая замена приложения недоступна.
Новые ключи, файлы `.env` и изменение конфигурации Codex не требуются.

### Выпуск версии

Версия приложения задаётся в `src/Cargo.toml`. После коммита изменений
владелец репозитория создаёт и отправляет соответствующий тег; например,
для первой версии форка:

```sh
git tag v0.3.4
git push origin master
git push origin v0.3.4
```

Workflow Release проверяет код, собирает оба macOS-пакета и создаёт черновик
релиза с `manifest.json` и SHA-256. Опубликуйте проверенный черновик на странице
Releases: только после публикации версия становится доступна обновлятору.
Версии навыка выпускаются отдельно, как описано в его README.

## CONCAT_MCP_ROOTS

- Назначение: разрешить запись проектов, кадров и экспорта в указанных папках,
  в том числе в live-окне вне домашнего каталога.
- Формат: список абсолютных путей с разделителем ОС (`:` на macOS/Linux,
  `;` на Windows).
- Обязательность: необязательна; передаётся окружению процесса GUI при запуске.
- По умолчанию: запись ограничена домашним каталогом пользователя.
- Пример: `CONCAT_MCP_ROOTS=/var/www/my-video`.

Выбор рабочей папки в чате заменяет разрешённые папки записи для текущего
live-окна одним выбранным каталогом. Настройки приложения и окружение
не переписываются. Выбор не включает Automation, если её отключили вручную.

Проверки разработки: `cargo fmt --all --check`, тесты затронутых crates и
`cargo clippy --all-targets -- -D warnings`. Сохранённые исходные проверки
и сведения о базовом редакторе приведены ниже.

---

<p align="center">
  <img src="https://cdn.jsdelivr.net/gh/jub0t/Concat@main/assets/concat-mark-dark.png" alt="Concat" width="100" />
</p>

<h1 align="center">Concat — исходный редактор</h1>
<h3 align="center">The free, open-source, cross-platform video editor.</h3>

<div align="center">

<img src="https://cdn.jsdelivr.net/gh/jub0t/Concat@main/assets/editor.png" alt="Concat editor" width="100%" />

<p align="center">
  <a href="https://github.com/jub0t/Concat/releases"><img src="https://img.shields.io/github/downloads/jub0t/concat/total?style=flat&logo=github&logoColor=F8F8F8&label=Downloads&labelColor=212123&color=0568FD" alt="Total Downloads" /></a>
  <a href="https://github.com/jub0t/Concat/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/jub0t/Concat/ci.yml?style=flat&logo=githubactions&logoColor=F8F8F8&label=Build&labelColor=000000" alt="Build Status" /></a>
  <a href="https://github.com/jub0t/Concat/releases"><img src="https://img.shields.io/badge/Version-0.2.10-0568FD?style=flat&logo=semver&logoColor=F8F8F8&labelColor=212123" alt="Concat Version 0.2.10" /></a>
  <a href="https://discord.gg/DVuPfpXfqP"><img src="https://img.shields.io/badge/Discord-Join%20the%20server-5865F2?style=flat&logo=discord&logoColor=F8F8F8&labelColor=000000" alt="Join Concat Discord" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-AGPL%20v3-0568FD?style=flat&logo=gnu&logoColor=F8F8F8&labelColor=212123" alt="License: AGPL-3.0-or-later" /></a>
</p>

<p align="center">
  <a href="https://www.star-history.com/jub0t/concat"><img src="https://api.star-history.com/badge?repo=jub0t/concat&type=rank" alt="Star History Rank" height="62" /></a>
  <a href="https://trendshift.io/repositories/191379?utm_source=trendshift-badge&amp;utm_medium=badge&amp;utm_campaign=badge-trendshift-191379" target="_blank" rel="noopener noreferrer"><img src="https://trendshift.io/api/badge/trendshift/repositories/191379/daily?language=Rust" alt="jub0t%2FConcat | Trendshift" width="250" height="55"/></a>
  <a href="https://concat.standalones.org/#download"><img src="assets/download_button.svg" alt="Download Concat" width="251" /></a>
</p>

**🇬🇧 English** · [🇩🇪 Deutsch](docs/README.de.md) · [🇪🇸 Español](docs/README.es.md) · [🇫🇷 Français](docs/README.fr.md) · [🇮🇹 Italiano](docs/README.it.md) · [🇳🇱 Nederlands](docs/README.nl.md) · [🇵🇱 Polski](docs/README.pl.md) · [🇧🇷 Português (Brasil)](docs/README.pt-BR.md) · [🇷🇺 Русский](docs/README.ru.md) · [🇺🇦 Українська](docs/README.uk.md) · [🇹🇷 Türkçe](docs/README.tr.md) · [🇦🇿 Azərbaycanca](docs/README.az.md) · [🇭🇷 Hrvatski](docs/README.hr.md) · [🇮🇩 Bahasa Indonesia](docs/README.id.md) · [🇻🇳 Tiếng Việt](docs/README.vi.md) · [🇹🇭 ไทย](docs/README.th.md) · [🇯🇵 日本語](docs/README.ja.md) · [🇰🇷 한국어](docs/README.ko.md) · [🇨🇳 简体中文](docs/README.zh-Hans.md) · [🇹🇼 繁體中文](docs/README.zh-TW.md) · [🇸🇦 العربية](docs/README.ar.md) · [🇮🇱 עברית](docs/README.he.md) · [🇮🇷 فارسی](docs/README.fa.md) · [🇮🇳 हिन्दी](docs/README.hi.md) · [🇵🇰 اردو](docs/README.ur.md) · [🇧🇩 বাংলা](docs/README.bn.md)

## Paid Sponsors

<table width="100%">
  <tr>
    <td align="center">
      <a href="https://proxyon.io/?ref=concat"><img src="https://cdn.jsdelivr.net/gh/jub0t/Concat@main/assets/proxyon_sponsor_logo.png" alt="Proxyon" width="80" /></a><br />
      <a href="https://proxyon.io/?ref=concat"><b>Proxyon</b></a><br />
      <sub>Pay-as-you-go proxies for developers<br />Use code <code>JUB0T</code> for 20% off</sub>
    </td>
      <td align="center">
      <a href="https://www.patreon.com/cw/jub0t"><b>Support Concat</b></a><br />
      <sub>Concat needs your donations to continue development,<br/>we have come too far to back down now 😢</sub>
    </td>
  </tr>
</table>

</div>


## About

Concat is a free, open-source video editor for macOS, Windows, Linux and Android. It covers what most edits need: auto-captions, text-to-speech, background removal, keyframe animation, effects and titles, multi-track cutting, 4K export. With none of the catches: no watermark, no account, no subscription, no upload.

The editor's media processing runs on your machine. The models for captions, voices and cutout download once from Settings and work offline after that. The optional Codex chat in this fork uses the provider configured in Codex CLI; prompts and tool results, including inspected frames, can be sent to that provider.

**Good for:** TikTok, Reels and Shorts, YouTube videos, tutorials and screen recordings, podcast clips, memes.

**Also for machines:** a built-in MCP server for AI agents (`concat --mcp`, [set up](docs/MCP.md)) and a JSON-RPC API for scripts, so they can cut video with it too.

## Highlights

- 🚫 **The base editor has no watermark or account requirement.** The optional Codex panel uses your Codex account and provider.
- 🔒 **Local media processing and export.** The optional Codex chat requires its configured provider.
- 💬 **Auto-captions.** Made on your machine. Pick a model size, get styled captions on the timeline.
- 🗣️ **Text-to-speech + voice cloning.** A free local voice on the desktop, or any voice from a few seconds of a recording. Phones are getting their own system voice.
- 🧍 **Background removal.** People, objects, or paint the mask yourself.
- 🎞️ **Keyframes.** Position, scale, rotation, opacity, volume, effect parameters. Curve editor built in.
- ✨ **100 effects, filters and transitions.** Live in the preview.
- ✂️ **Cut fast.** Split, trim, ripple, merge, freeze frame, speed. Magnetic timeline if you want it.
- 🎚️ **Multi-track, multi-timeline.** Overlays and picture-in-picture: stack video, stills and titles on as many tracks as you like. Several cuts in one project. Blend modes (screen, add, multiply…), crop, flips.
- 📝 **Titles.** Fonts, stroke, shadow, background plate. Presets to start from.
- 🎙️ **One-switch voice cleanup.** Denoise, enhance voice, level the loudness. Plus chipmunk, robot, telephone and friends.
- 📤 **Export.** H.264, HEVC, AV1. Up to 4K 60, 10-bit colour.
- ⚡ **Fast.** Proxies built in, so 4K scrubs smoothly.
- 🤖 **Scriptable.** An MCP server built in, so Claude and other AI agents can edit with it ([one line to set up](docs/MCP.md)), and a JSON-RPC API for scripts. A build from source adds gRPC and a command-line tool.
- 🖥️ **macOS, Windows, Linux, Android.** 26 languages. Same app, same project files.

## Download

The links in this section download the upstream editor; they do not include this fork's live mode or Codex panel. Concat Live packages are published in [this fork's Releases](https://github.com/steel0rat/concat-live/releases), or you can build from this repository using the instructions above. Upstream downloads:

1. **[The website](https://concat.standalones.org/#download)** hands you the right build for your machine. Start here.
2. **[GitHub Releases](https://github.com/jub0t/Concat/releases)** has every build for every platform, with installers, packages and checksums. For when you want to pick.

Concat is in **beta**: it works, and it still has edges. [Say so](https://github.com/jub0t/Concat/issues) when you find one.

**Platforms**

- ✅ **Windows** · x86_64 (ARM builds are paused, see [#326](https://github.com/jub0t/Concat/issues/326)). A setup and an `.msi`. If SmartScreen stops an unsigned build: **More info** › **Run anyway**
- ✅ **macOS** · Intel and Apple silicon. If macOS refuses to open an unsigned build: `xattr -dr com.apple.quarantine /Applications/Concat.app`
- ✅ **Linux** · x86_64 and ARM. `.deb`, `.rpm`, `.AppImage` and an Arch package. Also a Flatpak on **[Flatpark](https://flatpark.org/apps/app.concat.editor/)**, a community Flatpak remote that wraps the x86_64 `.deb` of each release and updates with it
- ✅ **Android** · phones and tablets
- ✅ **iOS / iPadOS** · iPhone and iPad, sideloaded

✅ Supported · 🚧 Work in progress · 🧪 To be tested

**System requirements** and the optional model sizes are on [the website](https://concat.standalones.org/guides/system-requirements).

**[Code signing policy](CODE_SIGNING.md)**: why the Windows builds are not signed yet, and what Concat sends over the network (nothing you did not ask for).

## Build from source

You need Rust 1.93 or newer (see `rust-version` in `src/Cargo.toml`), the FFmpeg 7+ development libraries, cmake and a C++ compiler.

- **macOS:** `brew install ffmpeg`
- **Windows:** from [FFmpeg's Windows downloads](https://ffmpeg.org/download.html#build-windows), take BtbN's `win64-gpl-shared` build (the one CI uses), unpack it and point `FFMPEG_DIR` at it
- **Linux:** use your distro's FFmpeg if it is version 7 or newer, otherwise use a BtbN `shared` build with `FFMPEG_DIR` like on Windows

Then:

```sh
cd src
cargo run -p concat          # the editor window
```

Other useful commands:

```sh
cargo build --profile app -p concat   # shipping: UI opt0, engine opt3, stripped
cargo test                            # run the test suite
cargo fmt --check && cargo clippy --all-targets -- -D warnings   # what CI checks
```

If you change text the interface shows, run `python3 scripts/locales.py` from the repo root so the string inventory stays in sync. Full details live in [CONTRIBUTING.md](CONTRIBUTING.md).

## How to Contribute

> [!IMPORTANT]
> The best way to contribute is to grab a build from the [Releases](https://github.com/jub0t/Concat/releases) page and use it: find where it breaks, and say where it could be better.
>
> Ready to write code? [CONTRIBUTING.md](./CONTRIBUTING.md) covers setup, the layout of the tree, the checks to run, and how contributions are licensed. Driving Concat from a script, a service or an agent? [The developer docs](https://concat.standalones.org/docs) cover the Concat API and its transports: JSON-RPC and gRPC; [docs/MCP.md](docs/MCP.md) connects an AI agent over MCP. [This Discussion](https://github.com/jub0t/Concat/discussions/3) is where the project was announced.
> 
> Contributors can claim a `@Contributor` role in the Discord server: just ask.

## Star History

<a href="https://www.star-history.com/?repos=jub0t%2Fconcat&type=date&releases=&legend=bottom-right">
 <img alt="Star History Chart" src="https://api.star-history.com/chart?repos=jub0t/concat&type=date&legend=bottom-right" />
</a>

## Contributors

<a href="https://github.com/jub0t/Concat/graphs/contributors">
  <img alt="Contributors" src="https://contrib.rocks/image?repo=jub0t/concat">
</a>

## Licence

Concat is free software under [AGPL-3.0-or-later](LICENSE). A commercial licence is available for closed-source use; see [LICENSING.md](LICENSING.md).

## DMCA

If you believe something in this repository, its releases, or a package or model it distributes infringes your copyright, email [jub0trd@gmail.com](mailto:jub0trd@gmail.com) with a description of the work you own, a link to where the infringing material appears here, your contact details, a statement that you believe in good faith the use is not authorised by you, your agent, or the law, and a statement, under penalty of perjury, that the notice is accurate and that you are the owner or authorised to act for the owner, signed with your physical or electronic signature; valid notices are acted on promptly, and anyone whose material is removed may send a counter-notice to the same address.
