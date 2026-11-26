import { getCanvas, setCanvas } from "@/shared/api/tauri";

export const WELCOME_CANVAS_CONTENT = `# Welcome to Buzz

This private channel is your home base for getting oriented. Fizz, Honey, and Pollen can help you learn the app, troubleshoot setup, and work through something you are building.

## Work with your agents

- Mention an agent when you want its help.
- Bring multiple agents into the same conversation when you want different perspectives.
- Keep decisions, progress, and results in the channel so everyone shares the same context.

## Try something

Bring the team something you are building, or give them a quick challenge to see how they work together.

## Get help

Ask the team a question here, or read the [Buzz user guide](https://github.com/block/buzz#readme).
`;

/**
 * The Welcome canvas for a build or deployment with NO starter personas (the
 * browser build, where personas are desktop pack files). Same structure, but
 * it must not promise Fizz, Honey, and Bumble or "your agents" — advertising
 * a team that cannot exist reads as broken, not welcoming.
 */
export const WELCOME_CANVAS_AGENT_FREE_CONTENT = `# Welcome to Buzz

This private channel is your home base for getting oriented: notes, questions, and anything you are working through can live here.

## Make it yours

- Keep decisions, progress, and results in your channels so everyone shares the same context.
- Use this canvas for anything worth keeping out of the message flow.

## Try something

Say hello in a public channel, or create a channel for something you are working on.

## Get help

Read the [Buzz user guide](https://github.com/block/buzz#readme).
`;

type WelcomeCanvasClient = {
  getCanvas: typeof getCanvas;
  setCanvas: typeof setCanvas;
};

/**
 * Seed the Welcome canvas without overwriting anything the user has written.
 * `content` lets the caller pick the copy that matches what actually got
 * provisioned (the agent-free variant when the starter team is unavailable).
 */
export async function ensureWelcomeCanvas(
  channelId: string,
  content: string = WELCOME_CANVAS_CONTENT,
  client: WelcomeCanvasClient = { getCanvas, setCanvas },
) {
  const existing = await client.getCanvas(channelId);
  // Nullish (not `!== null`) so an absent field can never masquerade as an
  // existing canvas — that exact mismatch silently skipped seeding before.
  if (existing.updatedAt != null || existing.author != null) {
    return false;
  }

  await client.setCanvas({ channelId, content });
  return true;
}
