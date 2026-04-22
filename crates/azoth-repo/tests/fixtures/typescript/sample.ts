// Synthetic 500+ LOC TypeScript fixture exercising every node shape
// the v2.1-C extractor classifies: function, class, method, interface,
// type alias, enum, plus nested / async / generic / abstract variants.
// Not meant to be runnable — tree-sitter parses best-effort and the
// extractor must remain panic-free on every construct below.

export const CONST_A: number = 1;
export let mutableB: string = "x";
export var legacyC: boolean = false;
const D_PRIVATE: readonly number[] = [1, 2, 3];
const E_ARRAY: Array<string> = ["a", "b"];

export function topFunction(x: number): number {
    return x + 1;
}

export async function asyncWorker(): Promise<void> {
    await Promise.resolve();
}

export function* generatorFunction(): Generator<number> {
    yield 1;
    yield 2;
}

export async function* asyncGenerator(): AsyncGenerator<string> {
    yield "a";
    yield "b";
}

function privateFunction<T>(items: T[]): T | undefined {
    return items[0];
}

function genericWithBound<T extends { id: number }>(item: T): number {
    return item.id;
}

function defaultParam(x: number = 42): number {
    return x;
}

function restParam(...args: string[]): number {
    return args.length;
}

function destructuredParam({ a, b }: { a: number; b: number }): number {
    return a + b;
}

function tupleReturn(): [number, string] {
    return [1, "x"];
}

function unionReturn(flag: boolean): string | number {
    return flag ? "s" : 1;
}

function overloadTarget(x: string): string;
function overloadTarget(x: number): number;
function overloadTarget(x: string | number): string | number {
    return x;
}

export class Widget {
    private value: number;
    public readonly label: string;
    protected secret: boolean = false;

    constructor(v: number, label: string) {
        this.value = v;
        this.label = label;
    }

    public getValue(): number {
        return this.value;
    }

    public setValue(v: number): void {
        this.value = v;
    }

    static make(v: number, label: string): Widget {
        return new Widget(v, label);
    }

    static get zero(): Widget {
        return new Widget(0, "zero");
    }

    get currentValue(): number {
        return this.value;
    }

    set currentValue(v: number) {
        this.value = v;
    }

    async fetchRemote(): Promise<number> {
        return this.value;
    }

    *iterate(): Generator<number> {
        yield this.value;
    }
}

export class Container<T> {
    private items: T[];

    constructor(initial: T[]) {
        this.items = initial;
    }

    add(item: T): void {
        this.items.push(item);
    }

    get(index: number): T | undefined {
        return this.items[index];
    }

    get size(): number {
        return this.items.length;
    }

    static empty<U>(): Container<U> {
        return new Container<U>([]);
    }
}

export abstract class BaseService {
    abstract name(): string;
    abstract execute(input: string): Promise<string>;

    describe(): string {
        return `service:${this.name()}`;
    }
}

export class ConcreteService extends BaseService {
    name(): string {
        return "concrete";
    }

    async execute(input: string): Promise<string> {
        return input.toUpperCase();
    }
}

class PrivateClassA {
    inner(): number {
        return 1;
    }
}

class PrivateClassB {
    step(): void {}
    another(): void {}
    third(): number {
        return 3;
    }
}

export interface Renderer {
    render(): string;
    id: number;
    readonly version: string;
}

export interface Comparable<T> {
    compareTo(other: T): number;
}

export interface Pair<A, B> {
    first: A;
    second: B;
    swap(): Pair<B, A>;
}

interface InternalListener {
    onEvent(name: string): void;
    onError(err: Error): void;
}

interface ChainedInterface extends Renderer, Comparable<ChainedInterface> {
    extra(): boolean;
}

export type WidgetId = string | number;
export type Callback = (value: number) => void;
export type AsyncCallback = (value: number) => Promise<void>;
export type StringRecord = Record<string, string>;
export type Optional<T> = T | null | undefined;
export type Pick2<T, K extends keyof T> = { [P in K]: T[P] };
export type FunctionMap = { [key: string]: (x: number) => number };
type PrivateAlias = readonly number[];

export enum Color {
    Red,
    Green,
    Blue,
}

export enum StringKind {
    Alpha = "alpha",
    Beta = "beta",
    Gamma = "gamma",
}

export const enum ConstEnum {
    One = 1,
    Two,
    Three,
}

enum PrivateEnum {
    A,
    B,
}

export namespace Geometry {
    export function area(w: number, h: number): number {
        return w * h;
    }

    export interface Point {
        x: number;
        y: number;
    }

    export class Rectangle {
        constructor(public w: number, public h: number) {}
        area(): number {
            return this.w * this.h;
        }
    }
}

module LegacyModule {
    export function noop(): void {}

    export class Holder {
        value: number = 0;
    }
}

declare const DECLARED_GLOBAL: number;
declare function declaredFn(x: string): string;
declare class DeclaredClass {
    field: number;
    method(): void;
}
declare namespace DeclaredNamespace {
    function inner(): void;
}

export function higherOrder(fn: (x: number) => number): (x: number) => number {
    return (x) => fn(fn(x));
}

const inlineLambda = (x: number): number => x + 1;
const inlineAsyncLambda = async (x: number): Promise<number> => x + 1;

export function useLambda(): number {
    return inlineLambda(1);
}

function chained(): Promise<number> {
    return Promise.resolve(1)
        .then((x) => x + 1)
        .then((x) => x * 2);
}

async function awaiter(): Promise<number> {
    const a = await Promise.resolve(1);
    const b = await Promise.resolve(2);
    return a + b;
}

export function withTryCatch(): number {
    try {
        return 1;
    } catch (e: unknown) {
        return 0;
    } finally {
        void 0;
    }
}

export function ifBranches(x: number): number {
    if (x < 0) {
        return -1;
    } else if (x === 0) {
        return 0;
    } else {
        return 1;
    }
}

export function forOf(items: number[]): number {
    let total = 0;
    for (const item of items) {
        total += item;
    }
    return total;
}

export function forIn(obj: Record<string, number>): number {
    let total = 0;
    for (const key in obj) {
        total += obj[key];
    }
    return total;
}

export function whileLoop(n: number): number {
    let i = 0;
    while (i < n) {
        i++;
    }
    return i;
}

export function doWhileLoop(n: number): number {
    let i = 0;
    do {
        i++;
    } while (i < n);
    return i;
}

export function switchDispatch(c: Color): string {
    switch (c) {
        case Color.Red:
            return "red";
        case Color.Green:
            return "green";
        case Color.Blue:
            return "blue";
        default:
            return "unknown";
    }
}

export function nestedFunctions(n: number): number {
    function inner(x: number): number {
        function deeper(y: number): number {
            return y + 1;
        }
        return deeper(x) * 2;
    }
    return inner(n);
}

export class NestedClassOuter {
    private inner: NestedClassOuter.Inner;

    constructor() {
        this.inner = new NestedClassOuter.Inner();
    }
}

export namespace NestedClassOuter {
    export class Inner {
        value: number = 1;
        bump(): number {
            return ++this.value;
        }
    }
}

export interface Options {
    verbose?: boolean;
    timeout?: number;
    retries?: number;
}

export function withOptions(opts: Options = {}): number {
    return opts.timeout ?? 30;
}

export type Result<T, E> = { ok: true; value: T } | { ok: false; error: E };

export function makeOk<T>(value: T): Result<T, never> {
    return { ok: true, value };
}

export function makeErr<E>(error: E): Result<never, E> {
    return { ok: false, error };
}

export interface EventTarget {
    addEventListener(type: string, listener: (e: Event) => void): void;
    removeEventListener(type: string, listener: (e: Event) => void): void;
    dispatchEvent(e: Event): boolean;
}

export class SimpleEmitter implements EventTarget {
    private handlers: Map<string, Array<(e: Event) => void>> = new Map();

    addEventListener(type: string, listener: (e: Event) => void): void {
        const arr = this.handlers.get(type) ?? [];
        arr.push(listener);
        this.handlers.set(type, arr);
    }

    removeEventListener(type: string, listener: (e: Event) => void): void {
        const arr = this.handlers.get(type);
        if (!arr) return;
        this.handlers.set(
            type,
            arr.filter((x) => x !== listener),
        );
    }

    dispatchEvent(e: Event): boolean {
        const arr = this.handlers.get(e.type);
        if (!arr) return false;
        for (const fn of arr) fn(e);
        return true;
    }
}

export abstract class Storage<T> {
    abstract save(key: string, value: T): Promise<void>;
    abstract load(key: string): Promise<T | undefined>;
    abstract remove(key: string): Promise<void>;

    async saveBatch(entries: Array<[string, T]>): Promise<void> {
        for (const [k, v] of entries) {
            await this.save(k, v);
        }
    }

    async loadBatch(keys: string[]): Promise<Array<T | undefined>> {
        const out: Array<T | undefined> = [];
        for (const k of keys) {
            out.push(await this.load(k));
        }
        return out;
    }
}

export class InMemoryStorage<T> extends Storage<T> {
    private backing: Map<string, T> = new Map();

    async save(key: string, value: T): Promise<void> {
        this.backing.set(key, value);
    }

    async load(key: string): Promise<T | undefined> {
        return this.backing.get(key);
    }

    async remove(key: string): Promise<void> {
        this.backing.delete(key);
    }

    size(): number {
        return this.backing.size;
    }
}

export interface Subscription {
    unsubscribe(): void;
}

export interface Observable<T> {
    subscribe(next: (value: T) => void): Subscription;
}

export class BehaviorSubject<T> implements Observable<T> {
    private value: T;
    private listeners: Array<(value: T) => void> = [];

    constructor(initial: T) {
        this.value = initial;
    }

    getValue(): T {
        return this.value;
    }

    next(value: T): void {
        this.value = value;
        for (const fn of this.listeners) fn(value);
    }

    subscribe(next: (value: T) => void): Subscription {
        this.listeners.push(next);
        next(this.value);
        return {
            unsubscribe: () => {
                this.listeners = this.listeners.filter((x) => x !== next);
            },
        };
    }
}

export function combine<A, B>(a: A, b: B): A & B {
    return { ...a, ...b } as A & B;
}

export function parseNumber(s: string): number | undefined {
    const n = Number(s);
    return Number.isNaN(n) ? undefined : n;
}

export function toJson(value: unknown): string {
    return JSON.stringify(value);
}

export function fromJson<T>(text: string): T {
    return JSON.parse(text) as T;
}

export type ReadonlyDeep<T> = {
    readonly [K in keyof T]: ReadonlyDeep<T[K]>;
};

export type Mutable<T> = {
    -readonly [K in keyof T]: T[K];
};
