import React, { useEffect, useState, useMemo, useRef } from 'react';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';

// project imports
const ToolResult = React.lazy(() => import('@components/tools/ToolResult'));
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import { useAuth } from '@utilities/auth';
import { updateURLSection } from '@utilities/url';
import { scrollToSection } from '@utilities/interactions';
import { getRepoResults, getResults } from '@thorpi/results';
import { ResultSourceContext } from '@components/tools/ResultSourceContext';
import { resultDisplayName } from '@utilities/runs';
import { OutputDisplayType, type Output } from '@models/results';

type ParsedResults = Record<string, Output[]>;

interface ResultsTableOfContentsProps {
  parsedResults: ParsedResults;
  inViewElements: string[];
}

const ResultsTableOfContents = ({ parsedResults, inViewElements }: ResultsTableOfContentsProps) => {
  const tocRef = useRef<HTMLElement>(null);

  useEffect(() => {
    const toc = tocRef.current;
    if (!toc || inViewElements.length === 0) return;

    const allSelected = toc.querySelectorAll<HTMLElement>('.selected');
    if (allSelected.length === 0) return;

    const tocRect = toc.getBoundingClientRect();
    const firstSelected = allSelected[0];
    const lastSelected = allSelected[allSelected.length - 1];

    const firstTop = firstSelected.getBoundingClientRect().top - tocRect.top + toc.scrollTop;
    const lastBottom = lastSelected.getBoundingClientRect().bottom - tocRect.top + toc.scrollTop;

    if (firstTop < toc.scrollTop) {
      toc.scrollTo({ top: firstTop, behavior: 'smooth' });
    } else if (lastBottom > toc.scrollTop + toc.clientHeight) {
      toc.scrollTo({ top: lastBottom - toc.clientHeight, behavior: 'smooth' });
    }
  }, [inViewElements]);

  return (
    <nav className="results-toc" ref={tocRef}>
      <ul className="ul no-bullets">
        {parsedResults &&
          typeof parsedResults === 'object' &&
          Object.keys(parsedResults)
            .sort()
            .map((image) => (
              <li key={`results-${image}-toc`} className="results-toc-item">
                <a
                  href={`#results-${image}`}
                  onClick={() => scrollToSection(`results-tab-${image}`)}
                  title={image}
                  className={`${inViewElements.includes(image) ? 'selected' : 'unselected'}`}
                >
                  {resultDisplayName(image)}
                </a>
                <hr className="m-1" />
              </li>
            ))}
      </ul>
    </nav>
  );
};

interface ResultsProps {
  // the file's SHA256, or the repo's URL when repo is set
  sha256: string;
  results: ParsedResults;
  setResults: (results: ParsedResults) => void;
  numResults: number;
  setNumResults: (num: number) => void;
  allowHashUpdate?: boolean;
  // whether the results belong to a repo instead of a file
  repo?: boolean;
}

const Results = ({ sha256, results, setResults, numResults, setNumResults, repo = false }: ResultsProps) => {
  const [loading, setLoading] = useState(false);
  const [inViewElements, setInViewElements] = useState<string[]>([]);
  const { checkCookie } = useAuth();

  // get results from API
  useEffect(() => {
    let isSubscribed = true;
    const fetchData = async () => {
      setLoading(true);
      const onError = () => {
        void checkCookie();
      };
      const resultsRes = repo ? await getRepoResults(sha256, onError) : await getResults(sha256, onError, {});
      if (resultsRes && 'results' in resultsRes && isSubscribed) {
        setNumResults(Object.keys(resultsRes.results).length);
        setResults(resultsRes.results);
      }
      setLoading(false);
    };
    void fetchData();
    return () => {
      isSubscribed = false;
    };
  }, [sha256, repo]);

  // update whether an element is in the viewport
  const updateInView = (inView: boolean, entry: string) => {
    if (inView) {
      setInViewElements((prev) => [...prev, entry].sort());
    } else {
      setInViewElements((prev) => prev.filter((element) => element != entry).sort());
    }
  };

  // tells the result tiles whether to read result files from a repo's routes
  const source = useMemo(() => ({ repo }), [repo]);

  const parsedResults = useMemo(() => {
    if (!results || typeof results !== 'object') return {};
    return Object.fromEntries(
      Object.entries(results).filter(([, image]) => {
        return !(image[0].display_type && image[0].display_type == OutputDisplayType.Hidden);
      }),
    );
  }, [results]);

  // the table of contents column only exists when there are results to list
  const hasToc = Object.keys(parsedResults).length > 0;
  return (
    <ResultSourceContext.Provider value={source}>
      <div id="results-tab" className={`navbar-scroll-offset results-container${hasToc ? '' : ' results-container-single'}`}>
        <LoadingSpinner loading={loading}></LoadingSpinner>
        {parsedResults && typeof parsedResults === 'object' && !loading && (
          <>
            <div>
              {numResults == 0 && !loading && (
                <>
                  {/* a repo's panel uses the same plain banner text as its sibling panels; the file page keeps its heading */}
                  {!repo && <br />}
                  <AlertBanner severity={Severity.Info}>
                    {repo ? 'No tool results available.' : <h3>No Tool Results Available</h3>}
                  </AlertBanner>
                </>
              )}
              {Object.keys(parsedResults)
                .sort()
                .map((image) => (
                  <ToolResult
                    key={image}
                    header={resultDisplayName(image)}
                    tool={image}
                    sha256={sha256}
                    updateInView={updateInView}
                    updateURLSection={updateURLSection}
                    results={parsedResults[image]}
                  />
                ))}
            </div>
            {hasToc && (
              <div className="results-toc-col">
                <ResultsTableOfContents parsedResults={parsedResults} inViewElements={inViewElements} />
              </div>
            )}
          </>
        )}
      </div>
    </ResultSourceContext.Provider>
  );
};

export default Results;
