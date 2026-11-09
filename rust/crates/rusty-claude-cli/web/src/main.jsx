import React from 'react'
import { createRoot } from 'react-dom/client'
import App from './App.jsx'
import { ClientErrorBoundary } from './shared/errorBoundary.jsx'
import './styles.css'

createRoot(document.getElementById('root')).render(
  <ClientErrorBoundary>
    <App />
  </ClientErrorBoundary>,
)
