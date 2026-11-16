# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "numpy==2.3.4",
#     "sherpa-onnx==1.13.8",
#     "whisper-normalizer==1.0.1",
# ]
# ///
"""Pronunciation eval: how well does Kokoro say tricky text?

Each case in cases.tsv is read through the normaliser (src/normalise.rs),
synthesised by the real voxtray-engine, transcribed back by Whisper, and the
transcript is scored by word error rate against the expected spoken form.

A dev tool. Nothing here ships. See docs/DEVELOPMENT.md for how to run it.

    uv run tests/pronunciation/eval.py               # dev split, normaliser on
    uv run tests/pronunciation/eval.py --raw         # baseline: no normaliser
    uv run tests/pronunciation/eval.py --split test  # the held-out cases
"""

from __future__ import annotations

import argparse
import collections
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request
import wave
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
CRATE = HERE.parent.parent  # src-tauri
CACHE = Path(os.environ.get("VOXTRAY_EVAL_CACHE", Path.home() / ".cache" / "voxtray-eval"))

# Pinned archives. Bumping one means updating its SHA-256 here.
MODELS = {
    # The same Kokoro the app installs (see src/download.rs). Upstream
    # re-uploaded the archive on 2026-10-26; the model files inside match the
    # earlier upload.
    "kokoro": {
        "url": "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kokoro-multi-lang-v1_0.tar.bz2",
        "sha256": "60703707bc05ccd48d60e1eb723f38b07d6e474795a0b6a588d7220400e0f7a4",
        "dir": "kokoro-multi-lang-v1_0",
    },
    # Whisper large-v3-turbo, int8, packaged for sherpa-onnx. (Distil-Whisper
    # large-v3.5 repeats itself on short clips under sherpa-onnx.)
    "whisper": {
        "url": "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-turbo.tar.bz2",
        "sha256": "b11acbbcd660b44a8e0df33724feb5aaa709cf65668f2823d59f656312544f22",
        "dir": "sherpa-onnx-whisper-turbo",
    },
}
WHISPER_PREFIX = "turbo"


# --- models ------------------------------------------------------------------


def ensure_model(name: str) -> Path:
    """Download, check and unpack a pinned model once. Returns its folder."""
    spec = MODELS[name]
    root = CACHE / "models"
    folder = root / spec["dir"]
    marker = folder / ".sha256"
    if marker.exists() and marker.read_text().strip() == spec["sha256"]:
        return folder
    root.mkdir(parents=True, exist_ok=True)
    archive = CACHE / f"{name}.tar.bz2"
    if not archive.exists() or sha256_file(archive) != spec["sha256"]:
        print(f"downloading {spec['url']}", file=sys.stderr)
        part = archive.with_suffix(".part")
        with urllib.request.urlopen(spec["url"]) as resp, open(part, "wb") as out:
            shutil.copyfileobj(resp, out, 1 << 20)
        got = sha256_file(part)
        if got != spec["sha256"]:
            part.unlink()
            sys.exit(f"{name}: SHA-256 {got} does not match the pinned {spec['sha256']}")
        part.replace(archive)
    print(f"unpacking {archive.name}", file=sys.stderr)
    with tarfile.open(archive) as tar:
        tar.extractall(root, filter="data")
    marker.write_text(spec["sha256"])
    return folder


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


# --- cases and the normaliser ------------------------------------------------


def load_cases(path: Path) -> list[dict]:
    lines = path.read_text(encoding="utf-8").rstrip("\n").split("\n")
    header = lines[0].split("\t")
    cases = []
    for line in lines[1:]:
        row = dict(zip(header, line.split("\t")))
        # Multi-line inputs are written with a literal \n.
        row["text"] = row["text"].replace("\\n", "\n")
        cases.append(row)
    return cases


def normalise(texts: list[str], off: str | None) -> list[str]:
    """Run the app's normaliser through `cargo run --example normalise`."""
    out = subprocess.run(
        ["cargo", "run", "-q", "--example", "normalise"] + (["--", "--off", off] if off else []),
        cwd=CRATE,
        input=json.dumps(texts),
        capture_output=True,
        text=True,
        check=True,
    )
    return json.loads(out.stdout)


# --- synthesis ---------------------------------------------------------------


def engine_exe() -> Path:
    if exe := os.environ.get("VOXTRAY_ENGINE_EXE"):
        return Path(exe)
    subprocess.run(["cargo", "build", "-q", "--release", "-p", "voxtray-engine"], cwd=CRATE, check=True)
    return CRATE / "target" / "release" / "voxtray-engine"


def synthesise(texts: list[str], voice: str) -> list[Path]:
    """The WAV for each text, from the cache or a fresh engine."""
    audio = CACHE / "audio" / voice
    audio.mkdir(parents=True, exist_ok=True)
    paths = [audio / f"{hashlib.sha256(t.encode()).hexdigest()[:24]}.wav" for t in texts]
    todo = [(t, p) for t, p in zip(texts, paths) if not p.exists()]
    if todo:
        model = ensure_model("kokoro")
        out_dir = Path(tempfile.mkdtemp(prefix="voxtray-eval-"))
        proc = subprocess.Popen(
            [str(engine_exe()), "--model-dir", str(model), "--out-dir", str(out_dir), "--voice", voice],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        ready = json.loads(proc.stdout.readline())
        if ready.get("type") != "ready":
            sys.exit(f"engine failed to start: {ready}")
        started = time.monotonic()
        for i, (text, path) in enumerate(todo):
            req = {"type": "synthesize", "id": i, "text": text, "voice": voice, "speed": 1.0}
            proc.stdin.write(json.dumps(req) + "\n")
            proc.stdin.flush()
            reply = json.loads(proc.stdout.readline())
            if reply.get("type") != "result":
                sys.exit(f"engine error: {reply}")
            shutil.move(reply["path"], path)
        proc.stdin.write('{"type":"quit"}\n')
        proc.stdin.close()
        proc.wait()
        shutil.rmtree(out_dir, ignore_errors=True)
        print(f"synthesised {len(todo)} cases in {time.monotonic() - started:.0f} s", file=sys.stderr)
    return paths


# --- transcription -----------------------------------------------------------


def transcribe(paths: list[Path]) -> list[str]:
    """Whisper's transcript of each WAV, cached by voice and file name."""
    cache_file = CACHE / f"transcripts-{WHISPER_PREFIX}.json"
    cache = json.loads(cache_file.read_text()) if cache_file.exists() else {}
    key = lambda p: f"{p.parent.name}/{p.name}"
    todo = [p for p in paths if key(p) not in cache]
    if todo:
        import sherpa_onnx

        model = ensure_model("whisper")
        rec = sherpa_onnx.OfflineRecognizer.from_whisper(
            encoder=str(model / f"{WHISPER_PREFIX}-encoder.int8.onnx"),
            decoder=str(model / f"{WHISPER_PREFIX}-decoder.int8.onnx"),
            tokens=str(model / f"{WHISPER_PREFIX}-tokens.txt"),
            language="en",
            task="transcribe",
            num_threads=min(os.cpu_count() or 4, 8),
        )
        started = time.monotonic()
        for p in todo:
            samples, rate = read_wav(p)
            # A little silence either side keeps Whisper from clipping words.
            pad = np.zeros(rate // 2, dtype=np.float32)
            stream = rec.create_stream()
            stream.accept_waveform(rate, np.concatenate([pad, samples, pad]))
            rec.decode_stream(stream)
            cache[key(p)] = stream.result.text.strip()
        cache_file.write_text(json.dumps(cache, indent=0, sort_keys=True))
        print(f"transcribed {len(todo)} cases in {time.monotonic() - started:.0f} s", file=sys.stderr)
    return [cache[key(p)] for p in paths]


def read_wav(path: Path) -> tuple[np.ndarray, int]:
    with wave.open(str(path)) as w:
        assert w.getsampwidth() == 2 and w.getnchannels() == 1
        data = np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16)
        return data.astype(np.float32) / 32768.0, w.getframerate()


# --- scoring -----------------------------------------------------------------
#
# The expected form and Whisper's transcript are both put into one canonical
# token form before they are compared. Whisper writes what it hears in its own
# house style ("$5", "API", "kokoro.rs"), so the symbols it uses are turned
# back into the words that must have been said to make it write them, and
# Whisper's own normaliser (numbers to digits, British to American spelling)
# runs on both sides. This step never sees the input text, only what was
# heard and what should have been, so it can't hide a bad reading.

_whisper_norm = None

# Symbols Whisper writes for spoken words.
SYMBOL_WORDS = [
    (r"°\s*C\b", " degrees celsius "),
    (r"°\s*F\b", " degrees fahrenheit "),
    (r"°", " degrees "),
    (r"±", " plus or minus "),
    (r"×", " times "),
    (r"§", " section "),
    (r"©", " copyright "),
    (r"&", " and "),
    (r"@", " at "),
    (r"#(?=\d)", " number "),
    (r"#", " hashtag "),
    (r"\+", " plus "),
    (r"==|=", " equals "),
    (r"(?<=\s)>(?=\s)", " greater than "),
    (r"(?<=\s)<(?=\s)", " less than "),
    (r"[/\\]", " slash "),
    (r"_", " underscore "),
    (r"(?<![\w-])--(?=\w)", " dash dash "),
]


def canonical(text: str) -> list[str]:
    global _whisper_norm
    if _whisper_norm is None:
        from whisper_normalizer.english import EnglishTextNormalizer

        _whisper_norm = EnglishTextNormalizer()
    s = text
    s = re.sub(r"\bPhD\b", "P H D", s)
    s = re.sub(r"\bCtrl\b", "control", s, flags=re.I)
    # Dotted initialisms ("U.S.", "Ph.D.", "a.m.") are letters said one by one.
    s = re.sub(
        r"\b((?:[A-Za-z]{1,2}\.){2,})",
        lambda m: " " + " ".join(m.group(1).replace(".", "").upper()) + " ",
        s,
    )
    # A dot inside a word ("kokoro.rs", "example.com") is said "dot".
    s = re.sub(r"(?<=[A-Za-z])\.(?=[A-Za-z])", " dot ", s)
    s = re.sub(r"(?<=\d)\.(?=[A-Za-z])", " dot ", s)
    # Clock times: "3:30" and "three thirty" both end up 330.
    s = re.sub(r"\b(\d{1,2}):(\d\d)(?!\d)", r"\1\2", s)
    s = re.sub(r"(\d)\s*([apAP])\.?[mM]\b\.?", r"\1 \2 M", s)
    # Letters stuck to digits: "RC2", "v1".
    s = re.sub(r"(?<=[A-Za-z])(?=\d)|(?<=\d)(?=[A-Z])", " ", s)
    s = re.sub(r"(?<=[A-Za-z]):(?=\w)", " colon ", s)
    # "24/7", "50/50": Whisper's slash between numbers isn't said.
    s = re.sub(r"(?<=\d)/(?=\d)", " ", s)
    for pattern, words in SYMBOL_WORDS:
        s = re.sub(pattern, words, s)
    # A dot that starts a name (".gitignore", "/.config") is said "dot".
    s = re.sub(r"(?:(?<=\s)|^)\.(?=[A-Za-z])", "dot ", s)
    # Unit symbols after a number ("16GB", "3.5 GHz") are the unit's name,
    # however it was said; plurals don't count.
    s = re.sub(r"\b(\d+)\s?h\s?(\d+)\s?m\b", r"\1 hour \2 minute", s)
    s = re.sub(UNIT_SYMBOLS, lambda m: f"{m.group(1)} {UNIT_NAMES[m.group(2)]}", s)
    s = re.sub(r"\b(" + "|".join(set(UNIT_NAMES.values())) + r")s\b", r"\1", s, flags=re.I)
    # "macOS", "iOS": a trailing capital run is letters.
    s = re.sub(r"(?<=[a-z])(?=[A-Z]{2,}s?\b)", " ", s)
    # Whisper sometimes writes what it heard as camelCase: "FetchAllItems".
    s = re.sub(r"(?<=[a-z])(?=[A-Z][a-z])", " ", s)
    # Money: "$3.50" is "three dollars fifty", not "three point five oh".
    s = re.sub(r"([$£€]\s?\d+)\.(\d\d)\b", r"\1 \2", s)
    # Capitalised acronyms ("API", "PRs") are letters: "A P I", "P R s".
    s = re.sub(r"\b([A-Z]{2,})(s?)\b", lambda m: " ".join(m.group(1)) + (" s" if m.group(2) else ""), s)
    # Whisper's normaliser drops these words next to numbers.
    s = re.sub(r"(?<![\w-])-(?=\d)", " minus ", s)
    s = re.sub(r"\b(minus|plus)\b", r" keep\1 ", s, flags=re.I)
    s = _whisper_norm(s)
    s = re.sub(r"keep(minus|plus)", r" \1 ", s)
    # It turns "one" into 1 in some places and not others.
    s = re.sub(r"\bone\b", "1", s)
    # Whisper writes "March 5" for "March fifth".
    s = re.sub(r"\b(" + MONTHS + r") (\d+)(?:st|nd|rd|th)\b", r"\1 \2", s)
    # Short names after "dot" (".co.uk", ".rs", ".md") and lower-case words
    # with no vowel ("npm", "bbc") are letters, whichever way Whisper wrote
    # them.
    s = re.sub(r"\bdot ([a-z]{2})\b", lambda m: "dot " + " ".join(m.group(1)), s)
    s = re.sub(r"\b[b-df-hj-np-tv-xz]{2,5}\b", lambda m: " ".join(m.group(0)), s)
    # Whisper's normaliser groups spoken digits inconsistently ("24 7" vs
    # "247"), so compare digit by digit. A decimal point stays a token.
    s = re.sub(r"(?<=\d)\.(?=\d)", " point ", s)
    s = re.sub(r"(\d)", r" \1 ", s)
    s = re.sub(r"([$£€¥¢%])", r" \1 ", s)
    s = re.sub(r"[^\w$£€¥¢%' ]", " ", s)
    s = s.replace("'", "")
    return s.split()


UNIT_NAMES = {
    "TB": "terabyte", "GB": "gigabyte", "MB": "megabyte", "KB": "kilobyte", "kB": "kilobyte",
    "ms": "millisecond", "km": "kilometer", "kg": "kilogram", "cm": "centimeter",
    "mm": "millimeter", "GHz": "gigahertz", "MHz": "megahertz", "kHz": "kilohertz",
    "mph": "miles per hour", "fps": "frames per second", "kWh": "kilowatt hour",
    "Mbps": "megabits per second", "Gbps": "gigabits per second", "ft": "feet",
    "L": "liter", "ml": "milliliter", "min": "minute", "mins": "minute", "hr": "hour",
    "hrs": "hour", "h": "hour",
}
UNIT_SYMBOLS = r"(\d)\s?(" + "|".join(sorted(UNIT_NAMES, key=len, reverse=True)) + r")\b"
MONTHS = "january|february|march|april|may|june|july|august|september|october|november|december"


def drop_repeats(text: str) -> str:
    """Whisper sometimes says a short clip twice or three times over. Keep
    the first time when the rest only repeats it."""
    words = text.split()
    for p in range(3, len(words) // 2 + 1):
        if all(words[i] == words[i - p] for i in range(p, len(words))):
            return " ".join(words[:p])
    return text


def edit_distance(ref: list[str], hyp: list[str]) -> int:
    prev = list(range(len(hyp) + 1))
    for i, r in enumerate(ref, 1):
        cur = [i] + [0] * len(hyp)
        for j, h in enumerate(hyp, 1):
            cur[j] = min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (r != h))
        prev = cur
    return prev[-1]


# Whisper's way of writing a correct reading, and the expected form: these
# must score the same. `--self-test` checks them without any models.
SAME = [
    ("16GB", "sixteen gigabytes"), ("3.5 GHz", "three point five gigahertz"),
    ("1TB", "one terabyte"), ("2L", "two liters"), ("5 min", "five minutes"),
    ("2h 30m", "two hours thirty minutes"), ("$3.50", "three dollars fifty"),
    ("3:30pm", "three thirty P M"), ("March 5, 2024", "March fifth, twenty twenty four"),
    ("readme.md", "readme dot M D"), ("bbc.co.uk", "B B C dot co dot U K"),
    ("npm", "N P M"), ("U.S.", "U S"), ("PRs", "P R s"), ("#42", "number forty two"),
    ("-5", "minus five"), ("24/7", "twenty four seven"), ("Ctrl+C", "control plus C"),
]
# Readings that are wrong, so must not score the same.
DIFFERENT = [
    ("3 slash 4", "three quarters"), ("e.g.", "for example"), ("5", "minus five"),
    ("dollar 5", "five dollars"), ("hash getting started", "getting started"),
]


def self_test() -> None:
    bad = [(a, b) for a, b in SAME if canonical(a) != canonical(b)]
    bad += [(a, b) for a, b in DIFFERENT if canonical(a) == canonical(b)]
    for a, b in bad:
        print(f"scorer: {a!r} vs {b!r}: {canonical(a)} / {canonical(b)}")
    if bad:
        sys.exit(1)
    print(f"scorer self-test: {len(SAME) + len(DIFFERENT)} pairs ok")


# --- report ------------------------------------------------------------------


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--raw", action="store_true", help="skip the normaliser (the baseline)")
    ap.add_argument("--off", metavar="RULES", help="switch off normaliser rules, e.g. acronyms,units")
    ap.add_argument("--split", choices=["dev", "test", "fresh", "all"], default="dev")
    ap.add_argument("--voice", default="af_heart")
    ap.add_argument("--cases", type=Path, default=HERE / "cases.tsv")
    ap.add_argument("--worst", type=int, default=15, help="how many worst cases to list")
    ap.add_argument("--json", type=Path, help="also write per-case results here")
    ap.add_argument("--self-test", action="store_true", help="check the scorer, then exit")
    args = ap.parse_args()
    if args.self_test:
        return self_test()

    cases = [c for c in load_cases(args.cases) if args.split == "all" or c["split"] == args.split]
    texts = [c["text"] for c in cases] if args.raw else normalise([c["text"] for c in cases], args.off)
    heard = transcribe(synthesise(texts, args.voice))

    total_err = total_ref = 0
    by_cat: dict[str, list[int]] = collections.defaultdict(lambda: [0, 0])
    results = []
    for case, said, hyp in zip(cases, texts, heard):
        ref_tokens, hyp_tokens = canonical(case["spoken"]), canonical(drop_repeats(hyp))
        err = edit_distance(ref_tokens, hyp_tokens)
        total_err += err
        total_ref += len(ref_tokens)
        by_cat[case["category"]][0] += err
        by_cat[case["category"]][1] += len(ref_tokens)
        results.append({
            "id": case["id"],
            "category": case["category"],
            "wer": err / max(len(ref_tokens), 1),
            "errors": err,
            "text": case["text"],
            "said": said,
            "heard": hyp,
            "expected": case["spoken"],
            "ref_tokens": " ".join(ref_tokens),
            "hyp_tokens": " ".join(hyp_tokens),
        })

    mode = "raw (no normaliser)" if args.raw else "normalised" + (f" without {args.off}" if args.off else "")
    print(f"\n{mode}, {args.split} split, {len(cases)} cases, voice {args.voice}")
    print(f"WER {100 * total_err / total_ref:.1f}%  ({total_err} errors / {total_ref} tokens)\n")
    print(f"{'category':<10} {'cases':>5} {'WER':>7}")
    counts = collections.Counter(c["category"] for c in cases)
    for cat in sorted(by_cat):
        err, n = by_cat[cat]
        print(f"{cat:<10} {counts[cat]:>5} {100 * err / n:>6.1f}%")
    print(f"\nworst {args.worst}:")
    for r in sorted(results, key=lambda r: (-r["wer"], r["id"]))[: args.worst]:
        if r["errors"] == 0:
            break
        print(f"  {r['id']:<11} {100 * r['wer']:>4.0f}%  {r['text']!r}")
        if r["said"] != r["text"]:
            print(f"{'':19}said:     {r['said']!r}")
        print(f"{'':19}heard:    {r['heard']!r}")
        print(f"{'':19}expected: {r['expected']!r}")
    if args.json:
        summary = {
            "mode": mode,
            "split": args.split,
            "wer": total_err / total_ref,
            "categories": {k: v[0] / v[1] for k, v in by_cat.items()},
            "cases": results,
        }
        args.json.write_text(json.dumps(summary, indent=1, ensure_ascii=False))


if __name__ == "__main__":
    main()
