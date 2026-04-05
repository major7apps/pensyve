#!/usr/bin/env python3
"""Download and prepare the LongMemEval_S dataset for the Pensyve benchmark harness.

Downloads the full LongMemEval dataset from HuggingFace (leowei/LongMemEval),
extracts the single-session (LongMemEval_S) split, and converts it to the
format expected by dataset.py (conversations.json + queries.json).

Usage:
    python benchmarks/longmemeval/prepare.py [--output-dir DIR] [--force]

Requirements:
    pip install huggingface_hub  (or: uv pip install 'pensyve-workspace[benchmarks]')
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

# Default output directory relative to this script.
_SCRIPT_DIR = Path(__file__).resolve().parent
_DEFAULT_OUTPUT_DIR = _SCRIPT_DIR / "data"

# HuggingFace dataset coordinates.
_HF_REPO_ID = "xiaowu0162/longmemeval"
_HF_FILENAME = "longmemeval_s"
# Fallback: the cleaned version may live under a different name.
_HF_FILENAME_ALT = "longmemeval_s.json"

# Difficulty mapping based on question_type.
_DIFFICULTY_MAP: dict[str, str] = {
    "single-session-user": "easy",
    "single-session-assistant": "easy",
    "single-session-preference": "medium",
    "multi-session": "hard",
    "temporal-reasoning": "hard",
    "knowledge-update": "hard",
}


def _download_dataset(output_dir: Path) -> Path:
    """Download the LongMemEval_S JSON file from HuggingFace.

    Returns the path to the downloaded JSON file.
    """
    try:
        from huggingface_hub import hf_hub_download
    except ImportError:
        print(
            "Error: huggingface_hub is not installed.\n"
            "Install it with:  uv pip install huggingface_hub\n"
            "Or install all benchmark deps:  uv pip install -e '.[benchmarks]'",
            file=sys.stderr,
        )
        sys.exit(1)

    raw_dir = output_dir / "raw"
    raw_dir.mkdir(parents=True, exist_ok=True)

    # Try the primary filename, then the alternative.
    for filename in [_HF_FILENAME, _HF_FILENAME_ALT]:
        print(f"  Attempting to download {filename} from {_HF_REPO_ID}...")
        try:
            downloaded = hf_hub_download(
                repo_id=_HF_REPO_ID,
                filename=filename,
                repo_type="dataset",
                local_dir=str(raw_dir),
            )
            print(f"  Downloaded: {downloaded}")
            return Path(downloaded)
        except Exception as e:
            # Check if this is a file-not-found type error.
            err_msg = str(e).lower()
            if "404" in err_msg or "not found" in err_msg or "entrynotfound" in err_msg:
                print(f"  {filename} not found, trying next...")
                continue
            # For other errors (network, auth), raise immediately.
            raise

    # If neither worked, list available files to help debug.
    print(
        f"\nError: Could not find {_HF_FILENAME} or {_HF_FILENAME_ALT} in {_HF_REPO_ID}.",
        file=sys.stderr,
    )
    print("Attempting to list available files...", file=sys.stderr)
    try:
        from huggingface_hub import HfApi

        api = HfApi()
        files = api.list_repo_files(repo_id=_HF_REPO_ID, repo_type="dataset")
        print(f"Available files: {files}", file=sys.stderr)
    except Exception:
        pass
    sys.exit(1)


def _convert_dataset(raw_path: Path, output_dir: Path) -> tuple[int, int]:
    """Convert raw LongMemEval_S JSON to conversations.json + queries.json.

    The raw format has one entry per *question*, where each entry contains:
      - question_id, question_type, question, answer
      - question_date, haystack_session_ids, haystack_dates
      - haystack_sessions: list of sessions (each session = list of turns)
      - answer_session_ids: which sessions contain the answer

    Multiple questions may share the same haystack sessions (same history).
    We deduplicate conversations and emit one conversation per unique session.

    Returns (num_conversations, num_queries).
    """
    print(f"  Reading raw data from {raw_path}...")
    with open(raw_path) as f:
        raw_data = json.load(f)

    print(f"  Found {len(raw_data)} evaluation instances.")

    # Collect all unique conversations and all queries.
    # Key: (question_id, session_index within that question's haystack) -> conversation
    # But sessions are shared across questions. We use content hashing to dedup.
    conversations_by_id: dict[str, dict] = {}
    queries: list[dict] = []

    for instance in raw_data:
        question_id = instance["question_id"]
        question_type = instance.get("question_type", "unknown")
        question = instance["question"]
        answer = instance["answer"]
        haystack_sessions = instance.get("haystack_sessions", [])
        haystack_session_ids = instance.get("haystack_session_ids", [])
        haystack_dates = instance.get("haystack_dates", [])
        answer_session_ids = instance.get("answer_session_ids", [])

        # Register each session as a conversation (dedup by session ID within
        # the instance). Session IDs are per-instance, so we prefix with
        # question_id to ensure global uniqueness only if needed.
        # However, many questions share the same haystack. We use the
        # haystack_session_ids as-is since they are meaningful identifiers.
        for idx, session in enumerate(haystack_sessions):
            if idx < len(haystack_session_ids):
                session_id = str(haystack_session_ids[idx])
            else:
                session_id = f"{question_id}_sess_{idx}"

            # Use a composite key: question_id + session_id to handle cases
            # where different questions have different session ID namespaces.
            conv_key = f"{question_id}:{session_id}"

            if conv_key not in conversations_by_id:
                # Clean up messages: remove 'has_answer' field, keep role+content.
                messages = []
                for turn in session:
                    messages.append(
                        {
                            "role": turn["role"],
                            "content": turn["content"],
                        }
                    )

                metadata: dict[str, str] = {}
                if idx < len(haystack_dates):
                    metadata["date"] = str(haystack_dates[idx])
                metadata["source_question_id"] = question_id
                metadata["session_id"] = session_id

                conversations_by_id[conv_key] = {
                    "conversation_id": conv_key,
                    "messages": messages,
                    "metadata": metadata,
                }

        # Determine the primary conversation_id for this query.
        # Use the first answer_session_id if available, else the first session.
        if answer_session_ids:
            primary_conv_id = f"{question_id}:{answer_session_ids[0]}"
        elif haystack_session_ids:
            primary_conv_id = f"{question_id}:{haystack_session_ids[0]}"
        else:
            primary_conv_id = f"{question_id}:sess_0"

        difficulty = _DIFFICULTY_MAP.get(question_type, "medium")

        queries.append(
            {
                "query_id": question_id,
                "question": question,
                "gold_answer": answer,
                "conversation_id": primary_conv_id,
                "difficulty": difficulty,
                "question_type": question_type,
            }
        )

    conversations = list(conversations_by_id.values())

    # Write output files.
    output_dir.mkdir(parents=True, exist_ok=True)

    conv_path = output_dir / "conversations.json"
    query_path = output_dir / "queries.json"

    print(f"  Writing {len(conversations)} conversations to {conv_path}")
    with open(conv_path, "w") as f:
        json.dump(conversations, f, indent=2, ensure_ascii=False)

    print(f"  Writing {len(queries)} queries to {query_path}")
    with open(query_path, "w") as f:
        json.dump(queries, f, indent=2, ensure_ascii=False)

    return len(conversations), len(queries)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Download and prepare the LongMemEval_S dataset for Pensyve benchmarks.",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=_DEFAULT_OUTPUT_DIR,
        help=f"Output directory for prepared data (default: {_DEFAULT_OUTPUT_DIR})",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Re-download and reconvert even if data already exists",
    )
    args = parser.parse_args()

    output_dir: Path = args.output_dir.resolve()
    conv_file = output_dir / "conversations.json"
    query_file = output_dir / "queries.json"

    # Check if already prepared.
    if conv_file.exists() and query_file.exists() and not args.force:
        print(f"Dataset already prepared at {output_dir}")
        print("Use --force to re-download and reconvert.")

        # Print summary of existing data.
        with open(conv_file) as f:
            num_convs = len(json.load(f))
        with open(query_file) as f:
            num_queries = len(json.load(f))
        print(f"  {num_convs} conversations, {num_queries} queries")
        return 0

    print("=" * 60)
    print("LongMemEval_S Dataset Preparation")
    print("=" * 60)

    # Step 1: Download.
    print("\nStep 1: Downloading from HuggingFace...")
    try:
        raw_path = _download_dataset(output_dir)
    except Exception as e:
        print(f"\nError downloading dataset: {e}", file=sys.stderr)
        print(
            "\nTroubleshooting:\n"
            "  - Check your internet connection\n"
            "  - The dataset may require authentication:\n"
            "    huggingface-cli login\n"
            "  - Or set HF_TOKEN environment variable",
            file=sys.stderr,
        )
        return 1

    # Step 2: Convert.
    print("\nStep 2: Converting to benchmark format...")
    try:
        num_convs, num_queries = _convert_dataset(raw_path, output_dir)
    except (KeyError, json.JSONDecodeError) as e:
        print(f"\nError converting dataset: {e}", file=sys.stderr)
        print(
            "The dataset format may have changed. "
            "Please check https://github.com/xiaowu0162/LongMemEval for updates.",
            file=sys.stderr,
        )
        return 1

    # Step 3: Summary.
    print(f"\n{'=' * 60}")
    print("Preparation complete!")
    print(f"{'=' * 60}")
    print(f"  Output directory: {output_dir}")
    print(f"  Conversations:    {num_convs}")
    print(f"  Queries:          {num_queries}")
    print("\nRun the benchmark with:")
    print(f"  python benchmarks/longmemeval/run.py --data-dir {output_dir}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
