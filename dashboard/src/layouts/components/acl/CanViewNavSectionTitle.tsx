// Copyright (c) RoochNetwork
// SPDX-License-Identifier: Apache-2.0

// ** React Imports
import { ReactNode } from 'react'

// ** Hooks Imports
import { useAuth } from 'src/hooks/useAuth'

// ** Types
import { NavSectionTitle } from 'src/@core/layouts/types'

interface Props {
  children: ReactNode
  navTitle?: NavSectionTitle
}

const CanViewNavSectionTitle = (props: Props) => {
  // ** Props
  const { children, navTitle } = props

  // ** Hook
  const auth = useAuth()

  if (auth.accounts || (navTitle && navTitle.auth === false)) {
    return <>{children}</>
  } else {
    return null
  }
}

export default CanViewNavSectionTitle
