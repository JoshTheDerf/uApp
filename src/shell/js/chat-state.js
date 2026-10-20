/* Chat state shared by the chat modules: chat-panel.js (the panel, sessions
 * and the composer) and chat-messages.js (the conversation renderer). */

export const cs = {
  session: "main",   // chat session shown in the panel
  sessions: [],      // sessions.list snapshot (chats + agent conversations)
  busySessions: [],  // sessions with a live run (app.info.aiSessions)
  approvals: [],     // pending tool approvals on this device
  questions: [],     // open ask_user prompts on this device
  aiBusy: false,     // this session has a live run
  draftText: "",     // the assistant's reply as it streams in
  blank: false,      // the app is still empty (app.info.blank)
};
