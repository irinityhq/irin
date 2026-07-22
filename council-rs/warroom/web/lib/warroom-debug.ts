/** Whether Librarian / War Room dev-only surfaces should render. */
export function warroomDebugEnabled(): boolean {
  return (
    process.env.NODE_ENV === "development"
    || process.env.NEXT_PUBLIC_WARROOM_DEBUG === "1"
  );
}