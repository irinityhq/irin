import { cn } from "@/lib/cn";
import type { Cabinet, DiscoverProvider, PrecedentMatch } from "@/lib/types";
import type { DesktopRuntimeMode } from "@/lib/tauri";
import type { GatewaySensitivity } from "@/lib/ws";
import CabinetSelector from "../CabinetSelector";
import ContextUploader from "../ContextUploader";
import MapScanner from "../MapScanner";
import type { ToastType } from "../Toast";
import { SessionControls } from "./SessionControls";
import { ShellFormHead } from "./ShellFormHead";
import { TopicPanel } from "./TopicPanel";
import { WargameHelp } from "./WargameHelp";
import type { ConveneMatter } from "./useConveneMatter";
import type { ConveneOptions } from "./useConveneOptions";

/** Center convene column: topic, cabinet, context/map, session controls. */
export function ConveneForm({
  variant,
  matter,
  options,
  precedent,
  cabinets,
  cabinetName,
  selectCabinet,
  availableIds,
  isWargameCabinet,
  cabinet,
  validatorProviders,
  governedAllowed,
  desktopMode,
  viaGateway,
  setViaGateway,
  sensitivity,
  setSensitivity,
  toast,
  wireMode,
  conveneButtonEl,
}: {
  variant: "standalone" | "shell";
  matter: ConveneMatter;
  options: ConveneOptions;
  precedent: PrecedentMatch[];
  cabinets: Cabinet[];
  cabinetName: string;
  selectCabinet: (name: string) => void;
  availableIds: string[] | null;
  isWargameCabinet: boolean;
  cabinet?: Cabinet;
  validatorProviders: DiscoverProvider[];
  governedAllowed: boolean;
  desktopMode: DesktopRuntimeMode | "detecting" | "unavailable";
  viaGateway: boolean;
  setViaGateway: (v: boolean) => void;
  sensitivity: GatewaySensitivity;
  setSensitivity: (v: GatewaySensitivity) => void;
  toast: (type: ToastType, message: string) => void;
  wireMode: string;
  conveneButtonEl: React.ReactNode;
}) {
  const {
    topic,
    setTopic,
    context,
    setContext,
    mapDir,
    setMapDir,
    setMapBrief,
  } = matter;
  const { blind } = options;

  const formBody = (
    <>
        <TopicPanel
          variant={variant}
          topic={topic}
          setTopic={setTopic}
          blind={blind}
          precedent={precedent}
        />

        {variant === "shell" ? (
          <div className="cg-convene-cabinet-block">
            <p className="cg-section-label mb-0">Cabinet</p>
            <CabinetSelector
              variant="command"
              embedded
              cabinets={cabinets}
              selected={cabinetName}
              onSelect={selectCabinet}
              providersAvailable={availableIds}
            />
          </div>
        ) : (
          <CabinetSelector
            cabinets={cabinets}
            selected={cabinetName}
            onSelect={selectCabinet}
            providersAvailable={availableIds}
          />
        )}

        {isWargameCabinet && (
          <WargameHelp variant={variant} cabinet={cabinet} />
        )}

        <div
          className={cn(
            variant === "shell"
              ? "cg-convene-options-block"
              : "grid grid-cols-1 md:grid-cols-2 gap-6",
          )}
        >
          {variant === "shell" && (
            <p className="cg-section-label mb-0">Context &amp; map</p>
          )}
          <ContextUploader value={context} onChange={setContext} />
          <MapScanner
            value={mapDir}
            onChange={setMapDir}
            onMapReady={setMapBrief}
          />
        </div>

        <SessionControls
          variant={variant}
          options={options}
          cabinet={cabinet}
          validatorProviders={validatorProviders}
          governedAllowed={governedAllowed}
          desktopMode={desktopMode}
          viaGateway={viaGateway}
          setViaGateway={setViaGateway}
          sensitivity={sensitivity}
          setSensitivity={setSensitivity}
          toast={toast}
        />
    </>
  );

  if (variant === "shell") {
    return (
      <>
        <ShellFormHead
          wireMode={wireMode}
          cabinet={cabinet}
          cabinetName={cabinetName}
          topic={topic}
          setTopic={setTopic}
          blind={blind}
          precedent={precedent}
        />
        <div className="cg-convene-body">{formBody}</div>
        <div className="cg-convene-sticky-bar">{conveneButtonEl}</div>
      </>
    );
  }

  return (
    <section className="col-span-12 lg:col-span-8 space-y-6">
      {formBody}
      <div className="flex gap-3">{conveneButtonEl}</div>
    </section>
  );
}
