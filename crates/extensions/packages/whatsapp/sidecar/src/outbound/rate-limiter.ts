// Token-bucket rate limits for outbound sends: one bucket per chat and one
// global bucket. A send consumes a token from both, or from neither.

interface Bucket {
	tokens: number
	updatedAt: number
}

export interface RateLimit {
	perMinute: number
	burst: number
}

export type RateDecision = { ok: true } | { ok: false; retryAfterMs: number }

const MAX_TRACKED_CHATS = 10_000

export class SendRateLimiter {
	private readonly chats = new Map<string, Bucket>()
	private readonly global: Bucket

	constructor(
		private readonly perChat: RateLimit,
		private readonly globalLimit: RateLimit,
		private readonly now: () => number = Date.now
	) {
		this.global = { tokens: globalLimit.burst, updatedAt: now() }
	}

	private refill(bucket: Bucket, limit: RateLimit, at: number): void {
		const elapsed = Math.max(0, at - bucket.updatedAt)
		bucket.tokens = Math.min(limit.burst, bucket.tokens + (elapsed * limit.perMinute) / 60_000)
		bucket.updatedAt = at
	}

	private waitMs(bucket: Bucket, limit: RateLimit): number {
		return Math.ceil(((1 - bucket.tokens) * 60_000) / limit.perMinute)
	}

	tryAcquire(chatId: string): RateDecision {
		const at = this.now()
		let chat = this.chats.get(chatId)
		if (!chat) {
			if (this.chats.size >= MAX_TRACKED_CHATS) this.evictFull(at)
			chat = { tokens: this.perChat.burst, updatedAt: at }
			this.chats.set(chatId, chat)
		}
		this.refill(chat, this.perChat, at)
		this.refill(this.global, this.globalLimit, at)
		const chatWait = chat.tokens >= 1 ? 0 : this.waitMs(chat, this.perChat)
		const globalWait = this.global.tokens >= 1 ? 0 : this.waitMs(this.global, this.globalLimit)
		const wait = Math.max(chatWait, globalWait)
		if (wait > 0) return { ok: false, retryAfterMs: wait }
		chat.tokens -= 1
		this.global.tokens -= 1
		return { ok: true }
	}

	/** Drops buckets that have refilled completely (they carry no state). */
	private evictFull(at: number): void {
		for (const [id, bucket] of this.chats) {
			this.refill(bucket, this.perChat, at)
			if (bucket.tokens >= this.perChat.burst) this.chats.delete(id)
		}
		if (this.chats.size >= MAX_TRACKED_CHATS) {
			const oldest = this.chats.keys().next().value
			if (oldest !== undefined) this.chats.delete(oldest)
		}
	}
}
