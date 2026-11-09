import React from 'react'

const ICONS = {
  plus: <><path d="M12 5v14M5 12h14" /></>,
  chevron: <path d="m6 9.5 6 6 6-6" />,
  shield: <path d="M12 3.2 19 5.8v5.7c0 4.3-3 7.6-7 9.3-4-1.7-7-5-7-9.3V5.8L12 3.2Z" />,
  mic: <><rect x="9.2" y="3" width="5.6" height="10.5" rx="2.8" /><path d="M5.8 11.5a6.2 6.2 0 0 0 12.4 0M12 17.7V21" /></>,
  wave: <><path d="M3.5 10v4M7 6.5v11M10.5 9v6M14 4.5v15M17.5 10v4" /><circle cx="20.5" cy="12" r="1" fill="currentColor" stroke="none" /></>,
  arrowUp: <><path d="M12 19V5" /><path d="m5.5 11.5 6.5-6.5 6.5 6.5" /></>,
  stop: <rect x="6.5" y="6.5" width="11" height="11" rx="2.5" fill="currentColor" stroke="none" />,
  laptop: <><rect x="4.5" y="5" width="15" height="10.5" rx="1.8" /><path d="M2.5 18.5h19" /></>,
  folder: <path d="M3 7.2c0-.9.7-1.6 1.6-1.6h4.3l2 2.4h7.7c.9 0 1.6.7 1.6 1.6v7.6c0 .9-.7 1.6-1.6 1.6H4.6c-.9 0-1.6-.7-1.6-1.6V7.2Z" />,
  paperclip: <path d="m20 11.2-8.1 8.1a4.8 4.8 0 0 1-6.8-6.8l8.6-8.6a3.2 3.2 0 0 1 4.5 4.5l-8.2 8.2a1.7 1.7 0 0 1-2.4-2.4l7.4-7.4" />,
  doc: <><rect x="5" y="3" width="14" height="18" rx="2" /><path d="M8.5 8h7M8.5 12h7M8.5 16h4.5" /></>,
  search: <><circle cx="11" cy="11" r="6.5" /><path d="m20 20-3.6-3.6" /></>,
  check: <path d="m5 12.5 4.5 4.5L19 7" />,
  terminal: <><path d="m5 8 4 4-4 4M12.5 16H19" /></>,
  settings: <><path d="M4 7h9M18 7h2M4 17h2M10 17h10" /><circle cx="16" cy="7" r="2" /><circle cx="8" cy="17" r="2" /></>,
  diff: <><path d="M6 5v14M12 7h6M12 12h6M12 17h6" /><circle cx="6" cy="5" r="1.5" fill="currentColor" stroke="none" /></>,
  cloud: <path d="M7 18h9.5a3.5 3.5 0 0 0 .6-6.9A5.5 5.5 0 0 0 6.3 9.8 4.1 4.1 0 0 0 7 18Z" />,
  x: <><path d="M6 6l12 12M18 6 6 18" /></>,
  pause: <><path d="M8 5v14M16 5v14" /></>,
  volume: <><path d="M4 9.5v5h3l4 3.5V6L7 9.5H4Z" /><path d="M15 9a4 4 0 0 1 0 6M17.5 6.5a7.5 7.5 0 0 1 0 11" /></>,
  volumeOff: <><path d="M4 9.5v5h3l4 3.5V6L7 9.5H4Z" /><path d="m15 9 6 6M21 9l-6 6" /></>,
}

export function Icon({ name, size = 18, className = '' }) {
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.7"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {ICONS[name] || null}
    </svg>
  )
}

export function Popover({ className = '', children }) {
  return <div className={`popover ${className}`} role="menu">{children}</div>
}

export function PopoverItem({ icon, glyph, label, hint, badge, selected = false, iconTone = '', onClick, disabled = false }) {
  return (
    <button type="button" className={`popover-item ${selected ? 'selected' : ''}`} onClick={onClick} disabled={disabled} role="menuitem">
      <span className={`popover-icon ${iconTone}`}>{glyph ? <span className="popover-glyph">{glyph}</span> : icon ? <Icon name={icon} size={15} /> : null}</span>
      <span className="popover-text">
        <span className="popover-label">{label}{badge ? <span className="model-badge">{badge}</span> : null}</span>
        {hint ? <span className="popover-hint">{hint}</span> : null}
      </span>
      {selected ? <Icon name="check" size={14} className="popover-check" /> : null}
    </button>
  )
}
