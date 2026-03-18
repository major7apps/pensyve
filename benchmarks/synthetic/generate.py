"""Generate synthetic benchmark conversations with planted facts."""
import json
import random


def generate_benchmark(num_conversations=50, facts_per_conversation=3, distractors_per_conversation=5):
    """Generate a benchmark dataset.

    Each conversation has:
    - A set of "planted facts" that should be remembered
    - Distractor messages that add noise
    - Verification questions with expected answers
    """
    benchmark = {
        "metadata": {
            "num_conversations": num_conversations,
            "facts_per_conversation": facts_per_conversation,
            "distractors_per_conversation": distractors_per_conversation,
        },
        "conversations": [],
        "queries": [],
    }

    # Fact templates: (fact_template, query_template, expected_keywords)
    fact_templates = [
        ("I prefer {preference} for my development environment",
         "What does the user prefer for development?", ["{preference}"]),
        ("My favorite programming language is {language}",
         "What programming language does the user prefer?", ["{language}"]),
        ("I work at {company} as a {role}",
         "Where does the user work?", ["{company}"]),
        ("I'm currently working on {project}",
         "What project is the user working on?", ["{project}"]),
        ("The best approach for {problem} is {solution}",
         "How should we handle {problem}?", ["{solution}"]),
        ("I usually deploy to {platform}",
         "What platform does the user deploy to?", ["{platform}"]),
        ("My team uses {tool} for {purpose}",
         "What tool does the team use for {purpose}?", ["{tool}"]),
        ("I had issues with {issue} and fixed it by {fix}",
         "How was {issue} fixed?", ["{fix}"]),
    ]

    # Values for template variables
    preferences = ["dark mode", "vim keybindings", "split panes", "minimal UI", "large fonts"]
    languages = ["Python", "Rust", "TypeScript", "Go", "Java", "Kotlin"]
    companies = ["Acme Corp", "TechStart", "DataFlow", "CloudNine", "BuildFast"]
    roles = ["senior engineer", "tech lead", "architect", "principal engineer"]
    projects = ["auth service rewrite", "API gateway", "data pipeline", "mobile app", "ML platform"]
    problems = ["slow queries", "memory leaks", "race conditions", "timeout errors", "auth failures"]
    solutions = ["adding an index", "connection pooling", "mutex locks", "retry with backoff", "token refresh"]
    platforms = ["AWS", "GCP", "Vercel", "Fly.io", "Railway"]
    tools = ["Docker", "Kubernetes", "Terraform", "GitHub Actions", "ArgoCD"]
    purposes = ["deployment", "CI/CD", "monitoring", "testing", "infrastructure"]
    issues = ["database deadlocks", "SSL certificate expiry", "DNS resolution", "container OOM", "API rate limiting"]
    fixes = ["reordering transactions", "auto-renewal setup", "switching to DoH", "increasing memory limits", "implementing backoff"]

    distractors = [
        "How's the weather today?",
        "Did you see the latest release notes?",
        "Let me check the documentation.",
        "That's an interesting approach.",
        "I'll need to think about that more.",
        "Can you explain that differently?",
        "Let me look into that.",
        "I remember reading about this somewhere.",
        "That reminds me of another project.",
        "Good point, I hadn't considered that.",
    ]

    for conv_idx in range(num_conversations):
        messages = []
        conv_facts = []
        conv_queries = []

        # Select random fact templates and fill them
        selected = random.sample(range(len(fact_templates)), min(facts_per_conversation, len(fact_templates)))

        for template_idx in selected:
            template, query_template, expected_keywords = fact_templates[template_idx]

            # Fill template variables
            variables = {}
            if "{preference}" in template: variables["preference"] = random.choice(preferences)
            if "{language}" in template: variables["language"] = random.choice(languages)
            if "{company}" in template: variables["company"] = random.choice(companies)
            if "{role}" in template: variables["role"] = random.choice(roles)
            if "{project}" in template: variables["project"] = random.choice(projects)
            if "{problem}" in template: variables["problem"] = random.choice(problems)
            if "{solution}" in template: variables["solution"] = random.choice(solutions)
            if "{platform}" in template: variables["platform"] = random.choice(platforms)
            if "{tool}" in template: variables["tool"] = random.choice(tools)
            if "{purpose}" in template: variables["purpose"] = random.choice(purposes)
            if "{issue}" in template: variables["issue"] = random.choice(issues)
            if "{fix}" in template: variables["fix"] = random.choice(fixes)

            fact_text = template.format(**variables)
            query_text = query_template.format(**variables)
            expected = [kw.format(**variables).lower() for kw in expected_keywords]

            messages.append({"role": "user", "content": fact_text})
            messages.append({"role": "agent", "content": "Got it, I'll remember that."})
            conv_facts.append({"text": fact_text, "keywords": expected})
            conv_queries.append({
                "query": query_text,
                "expected_keywords": expected,
                "source_conversation": conv_idx,
            })

        # Add distractors interspersed
        for _ in range(distractors_per_conversation):
            pos = random.randint(0, len(messages))
            messages.insert(pos, {"role": "user", "content": random.choice(distractors)})

        benchmark["conversations"].append({
            "id": conv_idx,
            "messages": messages,
            "planted_facts": conv_facts,
        })
        benchmark["queries"].extend(conv_queries)

    # Shuffle queries so they're not in conversation order
    random.shuffle(benchmark["queries"])

    return benchmark


if __name__ == "__main__":
    random.seed(42)  # reproducible
    data = generate_benchmark()
    with open("benchmarks/results/synthetic_benchmark.json", "w") as f:
        json.dump(data, f, indent=2)
    print(f"Generated {len(data['conversations'])} conversations with {len(data['queries'])} queries")
