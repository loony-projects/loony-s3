// Parses the LS3 XML responses crates/api/src/xml.rs produces. Uses the browser's
// built-in DOMParser -- no XML library needed, same "don't add a dependency you don't
// need" reasoning the backend used for hand-rolling its XML instead of pulling in a
// heavier serde adapter.

export interface BucketSummary {
  name: string;
  creationDate: string;
}

export interface ObjectSummary {
  key: string;
  lastModified: string;
  etag: string;
  size: number;
}

export interface ListBucketResult {
  objects: ObjectSummary[];
  commonPrefixes: string[];
  isTruncated: boolean;
  nextContinuationToken?: string;
}

export interface Ls3ErrorBody {
  code: string;
  message: string;
  requestId?: string;
}

function text(parent: Element, tag: string): string {
  return parent.getElementsByTagName(tag)[0]?.textContent ?? '';
}

/**
 * DOMParser never throws on bad input -- it returns a document containing a
 * <parsererror> element, which would otherwise read as an empty listing. Fail loudly.
 */
function parseXml(xml: string): Document {
  const doc = new DOMParser().parseFromString(xml, 'application/xml');
  if (doc.getElementsByTagName('parsererror').length > 0) {
    throw new Error('The server sent a malformed XML response');
  }
  return doc;
}

export function parseListAllMyBuckets(xml: string): BucketSummary[] {
  const doc = parseXml(xml);
  return Array.from(doc.getElementsByTagName('Bucket')).map((el) => ({
    name: text(el, 'Name'),
    creationDate: text(el, 'CreationDate'),
  }));
}

export function parseListBucketResult(xml: string): ListBucketResult {
  const doc = parseXml(xml);
  const root = doc.documentElement;

  const objects = Array.from(doc.getElementsByTagName('Contents')).map((el) => ({
    key: text(el, 'Key'),
    lastModified: text(el, 'LastModified'),
    etag: text(el, 'ETag').replace(/^"|"$/g, ''),
    size: Number(text(el, 'Size') || '0'),
  }));

  const commonPrefixes = Array.from(doc.getElementsByTagName('CommonPrefixes')).map((el) => text(el, 'Prefix'));

  const isTruncated = text(root, 'IsTruncated') === 'true';
  const token = doc.getElementsByTagName('NextContinuationToken')[0]?.textContent ?? undefined;

  return { objects, commonPrefixes, isTruncated, nextContinuationToken: token };
}

/** `<UploadId>` from a CreateMultipartUpload response. */
export function parseUploadId(xml: string): string {
  return parseXml(xml).getElementsByTagName('UploadId')[0]?.textContent ?? '';
}

/** The final object's ETag from a CompleteMultipartUpload response, quotes stripped. */
export function parseCompleteEtag(xml: string): string {
  return (parseXml(xml).getElementsByTagName('ETag')[0]?.textContent ?? '').replace(/^"|"$/g, '');
}

export function parseLs3Error(xml: string): Ls3ErrorBody | null {
  if (!xml.includes('<Error>')) return null;
  let doc: Document;
  try {
    doc = parseXml(xml);
  } catch {
    return null;
  }
  const root = doc.documentElement;
  if (root.tagName !== 'Error') return null;
  return {
    code: text(root, 'Code'),
    message: text(root, 'Message'),
    requestId: doc.getElementsByTagName('RequestId')[0]?.textContent ?? undefined,
  };
}
