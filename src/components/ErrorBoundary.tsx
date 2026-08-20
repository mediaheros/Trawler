import { Component, type ReactNode } from "react";
import { RefreshCw, TriangleAlert } from "lucide-react";

interface Props {
  children: ReactNode;
  /** identifies which region crashed in the error card */
  label?: string;
}

interface State {
  error: Error | null;
}

/** Contain a component crash to its region instead of blanking the app. */
export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error) {
    console.error(`[trawler] ${this.props.label ?? "component"} crashed:`, error);
  }

  render() {
    if (!this.state.error) return this.props.children;
    return (
      <div className="flex h-full min-h-[200px] flex-col items-center justify-center gap-3 p-8 text-center">
        <TriangleAlert size={26} className="text-warn" />
        <div className="text-[13.5px] font-semibold">
          {this.props.label ?? "This part of the app"} hit an error
        </div>
        <div className="max-w-[440px] font-mono text-[11px] leading-relaxed text-faint">
          {String(this.state.error?.message ?? this.state.error)}
        </div>
        <button
          type="button"
          onClick={() => this.setState({ error: null })}
          className="mt-1 inline-flex cursor-pointer items-center gap-1.5 rounded-lg bg-bg3 px-3 py-1.5 text-[12px] text-ink hover:bg-bg4"
        >
          <RefreshCw size={12} /> Try again
        </button>
      </div>
    );
  }
}
