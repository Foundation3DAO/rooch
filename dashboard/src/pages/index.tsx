// Copyright (c) RoochNetwork
// SPDX-License-Identifier: Apache-2.0

import { useEffect } from 'react'
import { useRouter } from 'next/router'
import { useAuth } from 'src/hooks/useAuth'
import Spinner from 'src/@core/components/spinner'

const Home = () => {
  const auth = useAuth()
  const router = useRouter()

  useEffect(() => {
    if (auth.accounts && router.route === '/') {
      router.replace('/dashboards/analytics')
    }
  }, [auth.accounts, router])

  return <Spinner sx={{ height: '100%' }} />
}

export default Home
