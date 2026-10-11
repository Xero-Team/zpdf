# Noto Sans SC test subset

`noto-sans-sc-subset.ttf` contains real Noto Sans SC outlines for space, `A`,
and `中文字体` (U+0020, U+0041, U+4E2D, U+4F53, U+5B57, U+6587). It is a
small rendering fixture, not a general-purpose CJK fallback font.

- Copyright: (c) 2014-2021 Adobe (http://www.adobe.com/), with Reserved Font
  Name 'Source'. This notice is also retained in the font's name table.
- License: SIL Open Font License 1.1, reproduced in `Noto-CJK-LICENSE`.
- Font version: `Version 2.004-H2;hotconv 1.0.118;makeotfexe 2.5.65603`.
- Upstream: https://github.com/notofonts/noto-cjk/tree/523d033d6cb47f4a80c58a35753646f5c3608a78
- Acquisition date: 2026-10-07.
- Subsetting: Google Fonts' public text-subsetting service, requested with
  `family=Noto Sans SC` and `text=中文字体A `; the returned TrueType program is
  included byte-for-byte without local modifications.
- Versioned source URL:
  https://fonts.gstatic.com/l/font?kit=k3kCo84MPvpLmixcA63oeAL7Iqp5IZJF9bmaG9_FnYxVZtSSGMjkFIr_yud9slLW&skey=cf0bb10b5602fdc3&v=v41
- Size: 2,568 bytes.
- SHA-256: `117828c692dbdfa9857ce4205a89da5f41cb70ec3b3aa7aced8de45c4de543a8`.
- License source:
  https://raw.githubusercontent.com/notofonts/noto-cjk/523d033d6cb47f4a80c58a35753646f5c3608a78/LICENSE

Normal tests and benchmarks use the checked-in file and require no network or
font-subsetting tool. To check acquisition and persistent offline reuse:

```sh
python3 tools/fetch_font.py \
  --url 'https://fonts.gstatic.com/l/font?kit=k3kCo84MPvpLmixcA63oeAL7Iqp5IZJF9bmaG9_FnYxVZtSSGMjkFIr_yud9slLW&skey=cf0bb10b5602fdc3&v=v41' \
  --sha256 117828c692dbdfa9857ce4205a89da5f41cb70ec3b3aa7aced8de45c4de543a8 \
  --max-bytes 2568 --cache-dir target/font-download-cache
```

Repeat with `--offline` to verify the persistent compressed cache. A changed
response fails the pinned digest check rather than silently replacing the
fixture. A full upstream font download was attempted but timed out on the
development host; no claim is made about full-font acquisition latency.
