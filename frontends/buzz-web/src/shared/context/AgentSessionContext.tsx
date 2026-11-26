import * as React from "react";

type AgentSessionContextValue = {
  onOpenAgentSession:
    | ((pubkey: string, channelId?: string | null) => void)
    | null;
  /**
   * The channel whose activity panel the provider opens in place. `null`
   * means the view renders no activity panel (a forum), so opens must
   * navigate; `undefined` means the provider did not say.
   */
  channelId?: string | null;
};

const AgentSessionContext = React.createContext<AgentSessionContextValue>({
  onOpenAgentSession: null,
});

export function AgentSessionProvider({
  channelId,
  children,
  onOpenAgentSession,
}: {
  channelId?: string | null;
  children: React.ReactNode;
  onOpenAgentSession: (pubkey: string, channelId?: string | null) => void;
}) {
  const value = React.useMemo(
    () => ({ channelId, onOpenAgentSession }),
    [channelId, onOpenAgentSession],
  );

  return (
    <AgentSessionContext.Provider value={value}>
      {children}
    </AgentSessionContext.Provider>
  );
}

export function useAgentSession() {
  return React.useContext(AgentSessionContext);
}
