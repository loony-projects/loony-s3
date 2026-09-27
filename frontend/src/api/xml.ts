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

function parseXml(xml: string): Document {
  return new DOMParser().parseFromString(xml, 'application/xml');
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

export function parseLs3Error(xml: string): Ls3ErrorBody | null {
  if (!xml.includes('<Error>')) return null;
  const doc = parseXml(xml);
  const root = doc.documentElement;
  if (root.tagName !== 'Error') return null;
  return {
    code: text(root, 'Code'),
    message: text(root, 'Message'),
    requestId: doc.getElementsByTagName('RequestId')[0]?.textContent ?? undefined,
  };
}
