/**
 * Fixed-size ring buffer. When full, new items overwrite the oldest.
 * Used for per-worker log history and activity tracking.
 */
export class RingBuffer<T> {
  private buf: (T | undefined)[]
  private head = 0
  private count = 0

  constructor(private capacity: number) {
    this.buf = new Array(capacity)
  }

  push(item: T): void {
    this.buf[this.head] = item
    this.head = (this.head + 1) % this.capacity
    if (this.count < this.capacity) this.count++
  }

  /** Return items oldest-first. */
  toArray(): T[] {
    if (this.count === 0) return []
    const start =
      this.count < this.capacity ? 0 : this.head
    const result: T[] = []
    for (let i = 0; i < this.count; i++) {
      result.push(this.buf[(start + i) % this.capacity] as T)
    }
    return result
  }

  get size(): number {
    return this.count
  }

  clear(): void {
    this.buf = new Array(this.capacity)
    this.head = 0
    this.count = 0
  }
}
