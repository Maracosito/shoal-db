#!/usr/bin/env python3
"""Minimal RAG pipeline on top of LagoonDB.

Pipeline: embed the question -> retrieve top passages from the demo-articles
namespace -> assemble a grounded context with source attributions -> answer.

Two answer modes:
  --llm none    (default) Extractive answer: ranks sentences from the
                retrieved passages by query-term overlap and returns the best
                ones, fully offline with no LLM dependency.
  --llm openai  Sends the grounded context to an OpenAI chat model.
                Requires `pip install openai` and OPENAI_API_KEY. The prompt
                instructs the model to answer only from the provided sources.

Usage:
    python demos/semantic-search/rag.py "What did the printing press change about books?"
    python demos/semantic-search/rag.py "How do bees tell each other where food is?" --llm openai
"""

from __future__ import annotations

import argparse
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from demos.common.embeddings import get_provider  # noqa: E402
from demos.common.lagoon_client import LagoonClient  # noqa: E402

DEFAULT_NAMESPACE = "demo-articles"
_SENTENCE_RE = re.compile(r"(?<=[.!?])\s+")
_WORD_RE = re.compile(r"[a-z0-9]+")

_STOPWORDS = {
    "the", "a", "an", "of", "and", "or", "to", "in", "on", "is", "are", "was",
    "were", "it", "its", "that", "this", "for", "with", "as", "by", "at", "be",
    "do", "does", "did", "what", "which", "who", "how", "why", "when", "where",
    "about", "from", "into", "their", "they", "than", "then", "so", "not",
}


def retrieve(client, provider, namespace: str, question: str, top_k: int):
    query_vector = provider.embed([question])[0]
    response = client.query(
        namespace,
        top_k=top_k,
        vector={"values": query_vector, "mode": "auto"},
        # Hybrid retrieval grounds RAG better than pure-vector with the
        # offline hash embedder; RRF needs no weight tuning.
        text={"query": question, "fields": {"title": 2.0, "body": 1.0}},
        fusion={"method": "rrf"},
        include_attributes=["title", "body", "category", "year"],
    )
    return response["results"]


def build_context(results) -> str:
    blocks = []
    for i, hit in enumerate(results, start=1):
        attrs = hit.get("attributes", {})
        blocks.append(
            f"[source {i}] {attrs.get('title')} "
            f"({attrs.get('category')}, {attrs.get('year')})\n{attrs.get('body')}"
        )
    return "\n\n".join(blocks)


# --------------------------------------------------------------- extractive


def _content_words(text: str) -> set[str]:
    return {w for w in _WORD_RE.findall(text.lower()) if w not in _STOPWORDS}


def extractive_answer(question: str, results, max_sentences: int = 3) -> str:
    """Pick the sentences from the retrieved passages that best cover the
    question's content words, weighting earlier (higher-ranked) sources."""
    query_words = _content_words(question)
    scored: list[tuple[float, str, str]] = []
    for rank, hit in enumerate(results):
        attrs = hit.get("attributes", {})
        title = attrs.get("title", "")
        for sentence in _SENTENCE_RE.split(attrs.get("body", "")):
            words = _content_words(sentence)
            if not words:
                continue
            overlap = len(query_words & words)
            if overlap == 0:
                continue
            # Overlap normalized by sentence length, with a rank bonus.
            score = overlap / (1.0 + 0.05 * len(words)) + (len(results) - rank) * 0.1
            scored.append((score, sentence.strip(), title))
    if not scored:
        return ("I could not find a relevant answer in the indexed corpus. "
                "Try rephrasing the question.")
    scored.sort(key=lambda t: t[0], reverse=True)
    lines, seen_sentences = [], set()
    for _, sentence, title in scored:
        if sentence in seen_sentences:
            continue
        seen_sentences.add(sentence)
        lines.append(f"- {sentence}  (from: {title})")
        if len(lines) >= max_sentences:
            break
    return "Based on the indexed corpus:\n" + "\n".join(lines)


# -------------------------------------------------------------------- llm


def openai_answer(question: str, context: str) -> str:
    try:
        from openai import OpenAI
    except ImportError:
        sys.exit("--llm openai requires: pip install 'openai>=1.30'")
    if not os.environ.get("OPENAI_API_KEY"):
        sys.exit("--llm openai requires OPENAI_API_KEY to be set")
    client = OpenAI()
    model = os.environ.get("LAGOON_OPENAI_CHAT_MODEL", "gpt-4o-mini")
    completion = client.chat.completions.create(
        model=model,
        messages=[
            {
                "role": "system",
                "content": (
                    "You answer questions using ONLY the provided sources. "
                    "Cite sources inline as [source N]. If the sources do not "
                    "contain the answer, say so plainly."
                ),
            },
            {
                "role": "user",
                "content": f"Sources:\n\n{context}\n\nQuestion: {question}",
            },
        ],
        temperature=0.2,
    )
    return completion.choices[0].message.content or ""


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("question")
    parser.add_argument("--namespace", default=DEFAULT_NAMESPACE)
    parser.add_argument("--provider", default=None)
    parser.add_argument("--top-k", type=int, default=4)
    parser.add_argument("--llm", choices=["none", "openai"], default="none")
    parser.add_argument(
        "--show-context", action="store_true",
        help="print the retrieved context before the answer",
    )
    args = parser.parse_args()

    provider = get_provider(args.provider)
    client = LagoonClient()

    results = retrieve(client, provider, args.namespace, args.question, args.top_k)
    if not results:
        sys.exit("No documents retrieved — did you run ingest.py first?")

    context = build_context(results)
    if args.show_context:
        print("=" * 70)
        print("RETRIEVED CONTEXT")
        print("=" * 70)
        print(context)
        print("=" * 70)

    print(f"\nQ: {args.question}\n")
    if args.llm == "openai":
        print(openai_answer(args.question, context))
    else:
        print(extractive_answer(args.question, results))

    print("\nSources:")
    for i, hit in enumerate(results, start=1):
        attrs = hit.get("attributes", {})
        print(f"  [{i}] {attrs.get('title')} (score {hit['score']:.4f})")


if __name__ == "__main__":
    main()
