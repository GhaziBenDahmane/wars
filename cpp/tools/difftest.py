#!/usr/bin/env python3
"""Compare the C++ solver with the Rust one on corpus prompts and random
mutations of them. Usage: difftest.py RUST_BINARY CPP_BATCH [variants-per-prompt]"""
import json, random, subprocess, sys
from concurrent.futures import ThreadPoolExecutor

rust, batch = sys.argv[1], sys.argv[2]
per = int(sys.argv[3]) if len(sys.argv) > 3 else 3
import os; random.seed(int(os.environ.get("SEED", 7)))
corpus = [json.loads(l)["prompt"] for l in open("../data/agentwars_prompts.jsonl") if l.strip()]
corpus = list(dict.fromkeys(corpus))

ALPHABET = "abcdeXYZ019 ,;.:()|'\"-+*/%^x_"
def mutate(p):
    p = list(p)
    for _ in range(random.randint(1, 3)):
        kind = random.random()
        i = random.randrange(len(p))
        if kind < 0.3:
            p[i] = random.choice(ALPHABET)
        elif kind < 0.5:
            del p[i]
        elif kind < 0.7:
            p.insert(i, random.choice(ALPHABET))
        elif kind < 0.85:
            p[i] = p[i].swapcase()
        else:
            if p[i].isdigit():
                p[i] = random.choice("0123456789")
    return "".join(p)

def structured(p):
    choices = [
        lambda s: s.replace("TASK:", "TASK: ", 1),
        lambda s: s.upper(),
        lambda s: s.lower(),
        lambda s: s.replace(" | ANSWER", ". | ANSWER"),
        lambda s: s.replace("counting from 1", "counting from the end"),
        lambda s: s.replace("forward", "back"),
        lambda s: s.replace("tokens", "token"),
        lambda s: s.replace("You", "You never"),
        lambda s: s.replace("mod", "modulo"),
        lambda s: s.replace("longest", "shortest"),
        lambda s: s.replace("rows", "three rows"),
        lambda s: s.replace("1", "12345678901234567890123"),
        lambda s: s.replace(", then", " then"),
        lambda s: s.replace("TASK", "TEXT: héllo wörld | TASK"),
        lambda s: s.replace("vowel", "consonant"),
        lambda s: s.replace("clockwise", "anti-clockwise"),
    ]
    return random.choice(choices)(p)

prompts = list(corpus)
for p in corpus:
    for _ in range(per):
        prompts.append(mutate(p) if random.random() < 0.6 else structured(p))
prompts = [p.replace("\n", " ") for p in prompts if p.strip() and not p.startswith("-")]
prompts = list(dict.fromkeys(prompts))

out = subprocess.run([batch], input="\n".join(prompts) + "\n", capture_output=True, text=True).stdout.split("\n")
cpp = [None if l == "!" else l[1:] for l in out[: len(prompts)]]

def run_rust(p):
    r = subprocess.run([rust, "solve", "--", p], capture_output=True, text=True)
    return r.stdout.rstrip("\n") if r.returncode == 0 else None

with ThreadPoolExecutor(24) as pool:
    rs = list(pool.map(run_rust, prompts))

diffs = [(p, a, b) for p, a, b in zip(prompts, rs, cpp) if a != b]
solved = sum(1 for a in rs if a is not None)
print(f"{len(prompts)} prompts, {solved} solved by Rust, {len(diffs)} differences")
for p, a, b in diffs[:40]:
    print(f"rust={a!r} cpp={b!r}: {p}")
sys.exit(1 if diffs else 0)
