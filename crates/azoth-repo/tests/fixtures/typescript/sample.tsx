// Synthetic TSX fixture. Exercises JSX-bearing declarations so the
// TSX parser variant (`language_tsx()`) is covered end-to-end. Not
// runnable — tree-sitter parses best-effort and the extractor must
// stay panic-free on every shape below.

import * as React from "react";

interface Props {
    name: string;
    count?: number;
}

interface CardProps {
    title: string;
    children?: React.ReactNode;
}

export function Greeting({ name, count = 1 }: Props): React.JSX.Element {
    return (
        <div className="greeting">
            Hello {name}, count={count}
        </div>
    );
}

export function Card({ title, children }: CardProps): React.JSX.Element {
    return (
        <section>
            <h3>{title}</h3>
            <div>{children}</div>
        </section>
    );
}

export function EmptyComponent(): React.JSX.Element {
    return <></>;
}

export function ConditionalRender({ show }: { show: boolean }): React.JSX.Element {
    return show ? <span>yes</span> : <span>no</span>;
}

export function ListRender({ items }: { items: string[] }): React.JSX.Element {
    return (
        <ul>
            {items.map((item, i) => (
                <li key={i}>{item}</li>
            ))}
        </ul>
    );
}

export class Counter extends React.Component<{}, { n: number }> {
    state = { n: 0 };

    increment = (): void => {
        this.setState({ n: this.state.n + 1 });
    };

    render(): React.JSX.Element {
        return (
            <span>
                <button onClick={this.increment}>+</button>
                {this.state.n}
            </span>
        );
    }
}

export class ErrorBoundary extends React.Component<
    { children?: React.ReactNode },
    { error?: Error }
> {
    state: { error?: Error } = {};

    static getDerivedStateFromError(error: Error): { error: Error } {
        return { error };
    }

    componentDidCatch(error: Error, info: React.ErrorInfo): void {
        void info;
    }

    render(): React.JSX.Element {
        if (this.state.error) {
            return <div>Error: {this.state.error.message}</div>;
        }
        return <>{this.props.children}</>;
    }
}

export interface ThemeContextValue {
    mode: "light" | "dark";
    toggle(): void;
}

export const ThemeContext = React.createContext<ThemeContextValue>({
    mode: "light",
    toggle: () => {},
});

export function useTheme(): ThemeContextValue {
    return React.useContext(ThemeContext);
}

export type ButtonVariant = "primary" | "secondary" | "ghost";

interface ButtonProps {
    label: string;
    variant?: ButtonVariant;
    onClick?: () => void;
}

export function Button({ label, variant = "primary", onClick }: ButtonProps): React.JSX.Element {
    return (
        <button className={`btn btn-${variant}`} onClick={onClick}>
            {label}
        </button>
    );
}

export enum RenderMode {
    Fast,
    Slow,
    Lazy,
}

export function renderByMode(mode: RenderMode): React.JSX.Element {
    switch (mode) {
        case RenderMode.Fast:
            return <span>fast</span>;
        case RenderMode.Slow:
            return <span>slow</span>;
        case RenderMode.Lazy:
            return <span>lazy</span>;
        default:
            return <span>unknown</span>;
    }
}
