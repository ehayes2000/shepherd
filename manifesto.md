shepherd was an agent management system that I built in a past life. It was ok. It was based on a good idea but the implementation was bad

I've been using herdr and herdr gets a few key things right

1. An agent sidebar that shows a list of active agents with status
2. Remote machines
3. managed worktrees

I think herdr sucks in a lot of other ways. 

- It's management is opaque and it doesn't provide good hooks for worktrees. 
- There's no good cleanup mechanism so I end up with a million worktrees that never go away
- I can't easily search / open worktrees
- I often want a single orchestrator that can talk to my agents for me. This is possible but not straightforward / intended
- Its UI is bad
- I want tools for setting up loops. IE I want to be able to get events from different sources and trigger actions. Herdr does not support this
(I don't think)

Shepherd was underdeveloped and still had bugs in its terminal emulation / split management. It was too focused on being a multiplexer
and not focused enough on agent management. 

The good decisions that shepherd made were
1. rust 
2. ratatui

The idea was sound (as proved by herdr)

# Initial features
1. agent monitoring and integrations
  - figure out how to integrate with claude code and codex
    - status, alerts, paused, elicitation, all status updates 
    - prompts, mcps, config, resumption, etc
    - Ideally I can both individually configure each coding agent but also set a global config that 
      makes works for all my supported CLI agents
2. worktree management and configuration
  - I don't ever want to think about worktrees. I want to set my workspace (this should save and default on next open but also be easy to see / swap). 
  Then within a workspace new sessions should create a managed worktree. This managed worktree will go into a direcotry that's managed by shepherd. 
  It will trigger whatever hooks. The default behavior should be to pull the latest default branch from origin to create the worktree from
  - creating a new session creates a new worktree
  - Sessions can be renamed and auto named. We have a single sidebar on the left with a list of all sessions newest to oldest
3. Multi-machine
  - We need multi machine agents. Ideally this is decentralized (is this a time for https://www.iroh.computer/?) so that whichever machine
  I start shepherd on connects to its peers.
  - I should be able to drive shepherd from any machine with full support to use any machine as if shepherd were on that machine
  - I should be able to give my machines names
  - agents rows in the sidebar will be 2 lines:
    1. name
    2. machine name, pr 
 
# Development

I want to restart this project. We can take the primitives that worked in the original version and re-use them in 
the next version. 

I want to rewrite this so that this is a herdr that's in my control.

I want to restart this with an agent-first design philosophy this means that we should use meaningful tests and TDD 
for all features. We should use hexagonal architecture to ensure that all behavior is testable. 

The initial version will be a multiplexer that just swaps sessions. It will not handle splits or tabs


