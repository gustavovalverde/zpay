import { ShieldCheck } from "lucide-react";
import type { DemoView } from "../App";

interface DemoNavProps {
  activeView: DemoView;
  views: { id: DemoView; label: string }[];
  onSelectView: (view: DemoView) => void;
  network: string;
}

export function DemoNav({ activeView, views, onSelectView, network }: DemoNavProps) {
  return (
    <div className="demo-nav">
      {views.length > 1 && (
        <div className="demo-nav-tabs" role="radiogroup" aria-label="Demo view">
          {views.map((entry) => (
            <button
              key={entry.id}
              type="button"
              role="radio"
              aria-checked={activeView === entry.id}
              className={activeView === entry.id ? "is-selected" : ""}
              onClick={() => onSelectView(entry.id)}
            >
              {entry.label}
            </button>
          ))}
        </div>
      )}
      <div className="network-pill" aria-label={`Network ${network}`}>
        <ShieldCheck aria-hidden="true" size={16} />
        {network}
      </div>
    </div>
  );
}
