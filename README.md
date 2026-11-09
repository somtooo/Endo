<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/endo-mark-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/endo-mark-light.svg">
    <img src="assets/endo-mark-light.svg" alt="Endo logo" width="96" height="96">
  </picture>
</p>

<h1 align="center">Endo</h1>

<p align="center">
  <em>An experiment for the balanced.</em>
</p>

---

## Why Endo

Endo is an experiment for the balanced: the people who believe AI is a game changer but know that to do game-changing things with it, they **cannot outsource their understanding**.

The UX of most new agentic development environments encourages offloading your thinking to Opus (they have to; it means more profit), and that's not a bad thing: Opus can go from a single prompt to production. But you're not achieving any form of domain expertise without exercising the LLM in your head, and that's what the IDE was for. It was never just a place to write code; **it was your gym**, where you could read code, inspect memory, read PDFs, write a blog post. There's no reason the equipment in that gym can't be replaced with AI-powered versions: highlight the part of the *Raft paper* you don't get and ask Opus to explain it, or stop at a breakpoint and ask Opus why your struct with a `u64` and a `u8` takes 16 bytes instead of 9.

So Endo starts with a traditional IDE and sees how far we can push it toward the extreme without sacrificing the benefits the traditional IDE gave us: *a place to practice, to learn, to attain domain expertise.*

- If the day calls for full vibe coding, no problem, **Endo's got you.**
- If it calls for digging in, sweating and pushing the LLM in your head, **Endo's got you too**, and you won't be alone: Opus is a great workout partner.

> [!TIP]
> See [this post by Andrej Karpathy](https://x.com/karpathy/status/1756380066580455557).

## The current idea

> [!NOTE]
> This will evolve over time as the world changes, but for now Endo comprises three core ideas.

1. **A very friendly fork of [Zed](https://zed.dev).** Changes from upstream are synced nightly, and stable releases track upstream's, shipping a day after each Zed stable release.
2. **Rebases the reading features from [Suzuri](https://suzuri.ai/)**, another fork of Zed.
3. **First-class support for [opencode](https://opencode.ai/).** It forgoes the ACP layer and talks to the opencode API directly. So, just like the opencode TUI, Endo is a full opencode client, because that's what it takes to provide the best UX.
