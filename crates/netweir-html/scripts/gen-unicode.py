"""Writes src/unicode.rs, the property tables behind \\p{...} in pattern=.

    python3 scripts/gen-unicode.py

The data is the Unicode Character Database (Unicode License v3) at the
version below, the one test262 and current JavaScript engines use. Only
what ECMAScript's \\p{...} can name is kept: General_Category, Script,
Script_Extensions and the binary properties in ECMA-262's table of them.
Each table is sorted, merged code point ranges, written flat as lo, hi.
"""

import subprocess
import urllib.request

VERSION = "17.0.0"
BASE = f"https://www.unicode.org/Public/{VERSION}/ucd/"
MAX = 0x10FFFF

# ECMA-262's binary properties and the one alias each may have, as its
# table lists them. ASCII, Any and Assigned aren't in the UCD files.
BINARY = [
    ("ASCII", None),
    ("ASCII_Hex_Digit", "AHex"),
    ("Alphabetic", "Alpha"),
    ("Any", None),
    ("Assigned", None),
    ("Bidi_Control", "Bidi_C"),
    ("Bidi_Mirrored", "Bidi_M"),
    ("Case_Ignorable", "CI"),
    ("Cased", None),
    ("Changes_When_Casefolded", "CWCF"),
    ("Changes_When_Casemapped", "CWCM"),
    ("Changes_When_Lowercased", "CWL"),
    ("Changes_When_NFKC_Casefolded", "CWKCF"),
    ("Changes_When_Titlecased", "CWT"),
    ("Changes_When_Uppercased", "CWU"),
    ("Dash", None),
    ("Default_Ignorable_Code_Point", "DI"),
    ("Deprecated", "Dep"),
    ("Diacritic", "Dia"),
    ("Emoji", None),
    ("Emoji_Component", "EComp"),
    ("Emoji_Modifier", "EMod"),
    ("Emoji_Modifier_Base", "EBase"),
    ("Emoji_Presentation", "EPres"),
    ("Extended_Pictographic", "ExtPict"),
    ("Extender", "Ext"),
    ("Grapheme_Base", "Gr_Base"),
    ("Grapheme_Extend", "Gr_Ext"),
    ("Hex_Digit", "Hex"),
    ("IDS_Binary_Operator", "IDSB"),
    ("IDS_Trinary_Operator", "IDST"),
    ("ID_Continue", "IDC"),
    ("ID_Start", "IDS"),
    ("Ideographic", "Ideo"),
    ("Join_Control", "Join_C"),
    ("Logical_Order_Exception", "LOE"),
    ("Lowercase", "Lower"),
    ("Math", None),
    ("Noncharacter_Code_Point", "NChar"),
    ("Pattern_Syntax", "Pat_Syn"),
    ("Pattern_White_Space", "Pat_WS"),
    ("Quotation_Mark", "QMark"),
    ("Radical", None),
    ("Regional_Indicator", "RI"),
    ("Sentence_Terminal", "STerm"),
    ("Soft_Dotted", "SD"),
    ("Terminal_Punctuation", "Term"),
    ("Unified_Ideograph", "UIdeo"),
    ("Uppercase", "Upper"),
    ("Variation_Selector", "VS"),
    ("White_Space", "space"),
    ("XID_Continue", "XIDC"),
    ("XID_Start", "XIDS"),
]


def lines(name):
    """The data lines of a UCD file, split on ';' with comments dropped."""
    text = urllib.request.urlopen(BASE + name).read().decode("utf-8")
    for line in text.splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            yield [f.strip() for f in line.split(";")]


def span(field):
    lo, _, hi = field.partition("..")
    return int(lo, 16), int(hi or lo, 16)


def merge(ranges):
    out = []
    for lo, hi in sorted(ranges):
        if out and lo <= out[-1][1] + 1:
            out[-1][1] = max(out[-1][1], hi)
        else:
            out.append([lo, hi])
    return out


def complement(ranges):
    out, nxt = [], 0
    for lo, hi in merge(ranges):
        if lo > nxt:
            out.append((nxt, lo - 1))
        nxt = hi + 1
    if nxt <= MAX:
        out.append((nxt, MAX))
    return out


def by_value(name):
    """Code point ranges for each value of a two-field file."""
    table = {}
    for f in lines(name):
        if len(f) == 2:
            table.setdefault(f[1], []).append(span(f[0]))
    return table


aliases = {}  # property -> [[short, long, other aliases...]]
for f in lines("PropertyValueAliases.txt"):
    if f[0] in ("gc", "sc"):
        aliases.setdefault(f[0], []).append(f[1:])

gc = by_value("extracted/DerivedGeneralCategory.txt")

scripts = by_value("Scripts.txt")
long_to_short = {names[1]: names[0] for names in aliases["sc"]}
scripts = {long_to_short[k]: v for k, v in scripts.items()}
scripts["Zzzz"] = complement([r for v in scripts.values() for r in v])

# Script_Extensions as the change from Script: a listed code point has
# exactly the scripts listed for it, any other the one Script gives it.
listed, extra = [], {}
for f in lines("ScriptExtensions.txt"):
    listed.append(span(f[0]))
    for short in f[1].split():
        extra.setdefault(short, []).append(span(f[0]))

binary = {}
for name in [
    "PropList.txt",
    "DerivedCoreProperties.txt",
    "DerivedNormalizationProps.txt",
    "emoji/emoji-data.txt",
    "extracted/DerivedBinaryProperties.txt",
]:
    for k, v in by_value(name).items():
        binary.setdefault(k, []).extend(v)
binary["ASCII"] = [(0, 0x7F)]
binary["Any"] = [(0, MAX)]
binary["Assigned"] = complement(gc["Cn"])
binary = {name: merge(binary[name]) for name, _ in BINARY}


def xor(a, b):
    """The code points in exactly one of two merged range lists."""
    edges = {}
    for lo, hi in a + b:
        for e in (lo, hi + 1):
            edges[e] = edges.get(e, 0) ^ 1
    edges = sorted(e for e, odd in edges.items() if odd)
    return [(edges[i], edges[i + 1] - 1) for i in range(0, len(edges), 2)]


# Most binary properties are close to a General_Category value or another
# binary property: Uppercase is Lu and five more ranges, XID_Start is
# ID_Start give or take fifteen. Each is stored as its difference from the
# base that makes it smallest, which cuts the tables to under a quarter. A
# property only becomes a base once its own table is settled, so every
# chain ends at General_Category or a property stored whole.
categories = {k: merge(v) for k, v in gc.items()}
for group in "CLMNPSZ":
    categories[group] = merge([r for k, v in gc.items() if k[0] == group for r in v])
categories["LC"] = merge(gc["Lu"] + gc["Ll"] + gc["Lt"])
best = {}
for name in binary:
    best[name] = ("", binary[name])
    for base, ranges in categories.items():
        delta = xor(binary[name], ranges)
        if len(delta) < len(best[name][1]):
            best[name] = (base, delta)
stored = {}
while best:
    name = min(best, key=lambda n: len(best[n][1]))
    stored[name] = best.pop(name)
    for other in best:
        delta = xor(binary[other], binary[name])
        if len(delta) < len(best[other][1]):
            best[other] = (name, delta)

# Script=Unknown is what no script claims: unassigned, private use and
# surrogates, so it's stored against General_Category=Other.
assert merge(scripts["Zzzz"]) == merge(gc["Cn"] + gc["Co"] + gc["Cs"])
script_base = {"Zzzz": ("C", xor(merge(scripts["Zzzz"]), categories["C"]))}

total = 0


def flat(ranges):
    global total
    merged = merge(ranges)
    total += len(merged)
    return "&[" + ", ".join(f"{lo:#x}, {hi:#x}" for lo, hi in merged) + "]"


def names(xs):
    return "&[" + ", ".join(f'"{x}"' for x in xs) + "]"


out = [
    f"// Generated by scripts/gen-unicode.py from the Unicode {VERSION} UCD. Don't edit.\n",
    "\n",
    "//! Unicode property tables for `\\p{...}` in pattern=. Each table is\n",
    "//! sorted, merged ranges, flat: lo, hi, lo, hi. A table with a base\n",
    "//! holds the code points in exactly one of it and the base, which is a\n",
    "//! General_Category value or a binary property.\n",
    "\n",
    "/// The General_Category values that aren't groups, by short name. A\n",
    "/// one-letter group is the values starting with its letter, and LC is\n",
    "/// Lu, Ll and Lt.\n",
    "pub(crate) static GENERAL_CATEGORY: &[(&str, &[u32])] = &[\n",
]
for short in sorted(gc):
    out.append(f'    ("{short}", {flat(gc[short])}),\n')
out.append("];\n\n")
out.append("/// Every General_Category value's names, short name first.\n")
out.append("pub(crate) static GENERAL_CATEGORY_NAMES: &[&[&str]] = &[\n")
for n in aliases["gc"]:
    out.append(f"    {names(n)},\n")
out.append("];\n\n")
out.append("/// A Script value: its names, short name first; its base and code\n")
out.append("/// points; and the code points Script_Extensions adds to them.\n")
out.append(
    "pub(crate) type Script = (&'static [&'static str], &'static str, &'static [u32], &'static [u32]);\n\n"
)
out.append("pub(crate) static SCRIPTS: &[Script] = &[\n")
for n in aliases["sc"]:
    base, ranges = script_base.get(n[0], ("", scripts.get(n[0], [])))
    out.append(f'    ({names(n)}, "{base}", {flat(ranges)}, {flat(extra.get(n[0], []))}),\n')
out.append("];\n\n")
out.append("/// The code points whose Script_Extensions aren't just their Script.\n")
out.append(f"pub(crate) static SCRIPT_EXTENSIONS_LISTED: &[u32] = {flat(listed)};\n\n")
out.append("/// ECMAScript's binary properties: name, alias, base, code points.\n")
out.append("pub(crate) static BINARY: &[(&str, &str, &str, &[u32])] = &[\n")
for name, alias in BINARY:
    base, ranges = stored[name]
    out.append(f'    ("{name}", "{alias or name}", "{base}", {flat(ranges)}),\n')
out.append("];\n")

with open("src/unicode.rs", "w") as f:
    f.write("".join(out))
# The tables are plain number arrays, which rustfmt packs into full lines.
subprocess.run(["rustfmt", "--edition", "2024", "src/unicode.rs"], check=True)
print(total, "ranges")
