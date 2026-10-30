import { EventEmitter } from "events";
import { Clock, systemClock } from "./clock";

export const MAX_RETRIES = 3;

export interface Job {
  id: string;
  run(): Promise<void>;
}

export interface Retrying extends Job {
  attempts: number;
}

/** Runs jobs one at a time and reports each outcome. */
export class Scheduler extends EventEmitter implements Retrying {
  attempts = 0;
  private queue: Job[] = [];

  constructor(private clock: Clock = systemClock) {
    super();
  }

  /** Adds a job to the back of the queue. */
  enqueue(job: Job): void {
    this.queue.push(job);
    this.emit("queued", job.id);
  }

  async drain(): Promise<number> {
    let done = 0;
    for (const job of this.queue) {
      await this.runOne(job);
      done += 1;
    }
    return done;
  }

  private async runOne(job: Job): Promise<void> {
    this.attempts = this.clock.now();
    await job.run();
  }
}

export function backoffMs(attempt: number): number {
  return Math.min(1000 * 2 ** attempt, 30000);
}

export const describeJob = (job: Job): string => `job ${job.id}`;

describe("backoffMs", () => {
  it("caps the delay", () => {
    expect(backoffMs(20)).toBe(30000);
  });
});
