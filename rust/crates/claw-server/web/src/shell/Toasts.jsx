export function Toasts({ toasts }) {
  return (
    <div className="toasts">
      {toasts.map((toast) => (
        <div key={toast.id} className={`toast ${toast.kind}`}>
          <span>{toast.text}</span>
          {toast.action && (
            <button className="toast-action" onClick={() => toast.action.run()}>{toast.action.label}</button>
          )}
        </div>
      ))}
    </div>
  )
}
