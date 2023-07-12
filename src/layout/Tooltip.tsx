import type { ReactNode } from "react";
import * as TooltipPrimitive from "@radix-ui/react-tooltip";

export default function Tooltip({
  children,
  text,
  delay = 150,
  asChild = true,
}: {
  children: ReactNode;
  text: string;
  delay?: number;
  asChild?: boolean;
}) {
  return (
    <TooltipPrimitive.Provider delayDuration={delay}>
      <TooltipPrimitive.Root>
        <TooltipPrimitive.Trigger asChild={asChild}>
          {children}
        </TooltipPrimitive.Trigger>
        <TooltipPrimitive.Portal>
          <TooltipPrimitive.Content
            className="rounded-md text-gray-50 bg-gray-600 pl-3 pr-3 max-w-md z-50 cursor-pointer pointer-events-none"
            sideOffset={5}
          >
            {text}
            <TooltipPrimitive.Arrow className="fill-gray-600" />
          </TooltipPrimitive.Content>
        </TooltipPrimitive.Portal>
      </TooltipPrimitive.Root>
    </TooltipPrimitive.Provider>
  );
}

type TooltipProps = {
  text: string;
  tooltip: string;
};

export function TooltipLabel({ text, tooltip }: TooltipProps) {
  return (
    <Tooltip text={tooltip}>
      <label className="justify-self-start cursor-help">{text}</label>
    </Tooltip>
  );
}