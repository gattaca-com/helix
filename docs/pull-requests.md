# Pull request descriptions

Write PR descriptions for reviewers, not as a record of agent activity. Never add a `Validation` or `Testing` section or list commands and checks run; reviewers care about the change, not the agent workflow.

Keep descriptions concise and easy to skim. Cover only:
- what the PR is trying to achieve;
- the key implementation details needed to understand how it achieves that;
- measurements demonstrating the improvement when the PR makes a measurable claim.

Prefer short paragraphs or bullets. Avoid verbose prose, large sections, implementation trivia, and status commentary. The description should give a human reviewer enough context to evaluate the change without burying the important points.

### Explain the change without requiring the codebase

Write for an engineer who does not have the code open. Start with a few short, simple sentences explaining the problem, what changes, and why that helps. Concise means removing unnecessary detail, not packing more jargon into fewer words.

- Use complete sentences with a clear subject and action: "The receiver acknowledges each batch. The sender resends missing datagrams." Avoid strings of technical nouns, parenthetical qualifications, and semicolon-separated fragments.
- Explain cause and effect. "Readers scan the entire cached subtree while holding a lock, making writers wait" tells the reader more than "shared-cache update tail contention." Introduce code names only when they help locate or understand the change.
- Give each sentence one main point. Describe behaviour before naming the mechanism. Keep necessary technical terms, but explain unfamiliar benchmark labels and abbreviations where they appear.
- Interpret measurements: say which workload improves, which gets worse, and what the numbers measure. Include units in table headings. A table should support an explanation the reader can understand on its own.
- Keep methodology that affects interpretation, and say why it matters. For example: "Six workers run on six physical cores. At twelve workers, pairs share a physical core, so that result does not represent twelve independent cores." Explain what cache priming represents if it matters; a count of "priming variants" alone communicates little. Omit routine setup details unless they explain a result or limitation.

Before publishing, read the description without the diff. Can the reviewer explain what changed, why it helps, and what the evidence establishes without decoding shorthand? If not, rewrite it in plain sentences rather than adding more compressed detail.