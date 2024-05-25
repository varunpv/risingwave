/*
 * Copyright 2024 RisingWave Labs
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 */

import { useEffect, useState } from "react"
import useErrorToast from "../../hook/useErrorToast"

/**
 * Fetch data from the server and return the response.
 * @param fetchFn The function to fetch data from the server.
 * @param intervalMs The interval in milliseconds to fetch data from the server. If null, the data is fetched only once.
 * @param when If true, fetch data from the server. If false, do nothing.
 * @returns The response from the server.
 */
export default function useFetch<T>(
  fetchFn: () => Promise<T>,
  intervalMs: number | null = null,
  when: boolean = true
) {
  const [response, setResponse] = useState<T>()
  const toast = useErrorToast()

  // NOTE(eric): Don't put `fetchFn` in the dependency array. It might be a lambda function
  useEffect(() => {
    const fetchData = async () => {
      if (when) {
        try {
          const res = await fetchFn()
          setResponse(res)
        } catch (e: any) {
          toast(e)
        }
      }
    }
    fetchData()

    if (!intervalMs) {
      return
    }

    const timer = setInterval(fetchData, intervalMs)
    return () => clearInterval(timer)
  }, [toast, intervalMs, when])

  return { response }
}
