/**
 * Types for the uapp client library (`/uapp.js`), the API an app inside a
 * .uapp file uses to reach its own database, files and the shell around it.
 *
 * Load the library with `<script src="/uapp.js"></script>` (the server also
 * injects it into app pages that don't), then use the global `uapp`. For
 * editor hints, point a reference comment at this file:
 *
 *     /// <reference path="./uapp.d.ts" />
 *
 * (Download it from `/uapp.d.ts` while the app is open.)
 */

declare namespace Uapp {
  /** A value SQLite can store. BLOBs arrive as `{ b64 }`. */
  type SqlValue = string | number | null | { b64: string };

  /** One result row, keyed by column name. */
  type Row = Record<string, SqlValue>;

  interface QueryResult<R extends Row = Row> {
    columns: string[];
    rows: R[];
    /** Present when the statement was a write that also returned rows. */
    changes?: number;
  }

  interface ExecResult {
    /** Rows changed by the statement. */
    changes: number;
    /** Rowid of the inserted row, or null when nothing was inserted. */
    insert_id: number | null;
    /** Same as `insert_id`, in JS casing. */
    insertId: number | null;
    /** Set when the statement also returned rows (`... RETURNING`). */
    columns?: string[];
    rows?: Row[];
  }

  interface FileContent {
    /** The archive name the request resolved to. */
    name: string;
    /** The bytes, base64-encoded. */
    b64: string;
    /** The bytes as UTF-8 text, or null when they aren't valid UTF-8. */
    text: string | null;
  }

  /** Raw `files.list` result: rows are `[name, size, mtime, role]` arrays. */
  interface FileList {
    columns: ["name", "sz", "mtime", "role"];
    rows: [name: string, size: number, mtime: number, role: "app" | "data"][];
  }

  /** What `onChange` / `onFilesChange` callbacks receive. */
  interface ChangeEvent {
    type: "changes";
    /** Table rows changed. */
    data: boolean;
    /** Archive files changed. */
    files: boolean;
    chat: boolean;
    config: boolean;
  }

  /** What `onPresent` callbacks receive (a file someone asked every shell to show). */
  interface PresentEvent {
    type: "present";
    name: string;
    mode: "auto" | "raw" | "pretty" | "edit";
    /** Who asked: a user name, or "assistant". */
    by: string;
  }

  /** JSON-schema-ish description of one action parameter. */
  interface ActionParam {
    type?: "string" | "number" | "integer" | "boolean" | "object" | "array";
    description?: string;
    enum?: unknown[];
    /** Parameters are required unless this is false. */
    required?: boolean;
    [key: string]: unknown;
  }

  interface ActionOptions {
    /** What it does and returns. The assistant reads this. */
    description?: string;
    /** Parameters by name. Ignored when `schema` is given. */
    params?: Record<string, ActionParam>;
    /** A full JSON schema for the input, instead of `params`. */
    schema?: object;
    /** Read-only actions skip the approval prompt. */
    readonly?: boolean;
  }

  interface ToolInfo {
    name: string;
    description: string;
    /** Needs approval before it runs (in Manual mode, or always for MCP). */
    gated: boolean;
    kind: "builtin" | "app" | "mcp";
    enabled: boolean;
    server?: string;
  }

  interface ImportCsvOptions {
    /** The CSV's name in the archive, e.g. "data/sales.csv". */
    file: string;
    /** Target table (defaults to the file's base name). */
    table?: string;
    delimiter?: string;
    /** Whether the first line holds column names (default true). */
    header?: boolean;
    /** Empty an existing table before importing into it (default false). */
    replace?: boolean;
  }

  type PanelName = "chat" | "files" | "database" | "settings" | "tools";

  interface ToolbarState {
    /** Shown right now in this window. */
    visible: boolean;
    /** The saved default: the app opens with the toolbar hidden. */
    hidden: boolean;
    /** The saved toggle shortcut, e.g. "Mod+Alt+B" ("" for none). */
    shortcut: string;
    /** The open panel, or null. */
    panel: PanelName | null;
  }

  interface AppInfo {
    name: string;
    user: string;
    device: string;
    appId: string;
    [key: string]: unknown;
  }

  /** Call this to unsubscribe. */
  type Unsubscribe = () => void;

  /**
   * The error every failed call rejects with. `method` names the call;
   * `code` says why when known: "timeout" (no reply in time), "disconnected"
   * (the connection dropped mid-call), "no_host" (the page isn't running
   * inside the uapp shell) or "unknown_action". Errors the server reports
   * (a refused SQL statement, a missing file) have no code; read `message`.
   */
  class UappError extends Error {
    name: "UappError" | "UappTimeout" | "UappNoHost";
    code?: string;
    method?: string;
    /** Timeouts only: the call's id. */
    callId?: number;
  }

  interface Api {
    UappError: typeof UappError;

    /** Run a read query. Rows come back as objects keyed by column name. */
    query<R extends Row = Row>(sql: string, params?: SqlValue[]): Promise<QueryResult<R>>;
    /** The first row of a query, or null when there is none. */
    get<R extends Row = Row>(sql: string, params?: SqlValue[]): Promise<R | null>;
    /** Run one write statement in its own transaction. Pass values as params. */
    exec(sql: string, params?: SqlValue[]): Promise<ExecResult>;
    /** Run several statements (separated by `;`) in one transaction. */
    batch(sql: string): Promise<{ ok: true }>;

    readFile(name: string): Promise<FileContent>;
    writeFile(name: string, text: string): Promise<{ ok: true; name: string }>;
    listFiles(): Promise<FileList>;
    /** Import a CSV already in the archive into a table. */
    importCsv(opts: ImportCsvOptions): Promise<{ table: string; rows: number; columns: string[] }>;
    /**
     * Download a public http(s) URL into the archive. Addresses on this
     * machine or the local network are refused.
     */
    fetchUrl(url: string, name?: string): Promise<unknown>;

    /**
     * Register a named piece of business logic. UI code calls it with
     * `uapp.call(name, input)`, and the assistant calls the same code as
     * the tool `app__<name>`.
     */
    action<I = any, O = any>(name: string, opts: ActionOptions, handler: (input: I) => O | Promise<O>): void;
    /** Call one of this page's own actions directly. */
    call<O = any>(name: string, input?: object): Promise<O>;
    /** Call any tool the assistant has: builtins, `mcp__server__tool`, `app__name`. */
    tool(name: string, input?: object): Promise<unknown>;
    tools(): Promise<ToolInfo[]>;

    /** Run `cb` when this app's table data changes (from this page, another window or the assistant). */
    onChange(cb: (e: ChangeEvent) => void): Unsubscribe;
    /** Run `cb` when this app's files change. */
    onFilesChange(cb: (e: ChangeEvent) => void): Unsubscribe;
    /** Run `cb` when someone asks the shells to show a file. */
    onPresent(cb: (e: PresentEvent) => void): Unsubscribe;
    /**
     * Run `cb` just before this page goes away (an edit reloads it). Dispose
     * renderers here; WebGL contexts are released for you either way.
     */
    onTeardown(cb: () => void): Unsubscribe;
    /** Run `cb` once `el` is visible (right away if it already is). */
    whenVisible(el: Element, cb: () => void): void;
    /** Ask every open shell to show a file from the archive. */
    presentFile(name: string, mode?: "auto" | "raw" | "pretty" | "edit"): Promise<{ ok: true }>;

    /** The bar uapp puts around this app. show/hide/toggle change this window only and save nothing. */
    toolbar: {
      /** Returns false when no shell is hosting this page. */
      show(): boolean;
      hide(): boolean;
      toggle(): boolean;
      /** Null when no shell is hosting this page. */
      state(): Promise<ToolbarState | null>;
      /** How the app opens, saved in the .uapp: `{hidden, shortcut}`. */
      setDefault(opts: { hidden?: boolean; shortcut?: string }): Promise<unknown>;
    };
    /** The panels beside the app. Opening one closes the others and shows the toolbar. */
    panel: {
      open(name: PanelName): boolean;
      close(name: PanelName): boolean;
      toggle(name: PanelName): boolean;
      /** The open panel's name, or null. */
      state(): Promise<PanelName | null>;
    };

    /** Low-level call to any server method. */
    rpc<T = unknown>(method: string, params?: object): Promise<T>;
    /** Milliseconds before a call with no reply fails with code "timeout" (default 60000). */
    rpcTimeout?: number;

    /** Filled in once `ready` resolves. */
    user: string | null;
    device: string | null;
    /** Resolves with the app's info once connected. */
    ready: Promise<AppInfo>;
  }
}

declare const uapp: Uapp.Api;
interface Window { uapp: Uapp.Api }
