unit CDAPI.Adapter.Data;

interface

uses
  SysUtils,
  System.Variants,
  mormot.core.base,
  mormot.core.os,
  mormot.core.text,
  mormot.core.unicode,
  mormot.core.datetime,
  mormot.core.json,
  mormot.core.variants,
  mormot.db.sql,
  CDAPI.Database.SV1020,
  CDAPI.Adapter.Metadata.Cache,
  CDAPI.Adapter.Metadata.Types,
  CDAPI.Adapter.Data.Types,
  CDAPI.Adapter.Data.LoadOptions;

type
  EUnresolvedMacros = class(Exception);
  EUsageNotFound = class(Exception);
  EDatabaseUnreachable = class(Exception);
  ERecordNotFound = class(Exception);
  eValidationError = class(Exception);

  tFieldNameMap = record
    DatasetName: RawUtf8;
    PhysicalName: RawUtf8;
    FieldType: Integer;
  end;
  TFieldNameMapArray = array of TFieldNameMap;

  TDataAdapter = class
  private
    fSV1020DB: TCDAPISV1020DB;
    FMetadataCache: TMetadataCache;
    FObject: TObject;
    function GetAllowedFields(aUsageId: Integer): TFieldNameMapArray;
    function RequeryRow(aConn: TSqlDBConnection;
      const aUsageDef: TUsageDef;
      const aAllowedFields: TFieldNameMapArray;
      const aPKFields: TRawUtf8DynArray;
      const aPKValues: TDocVariantData): RawJson;
  public
    constructor Create(aSV1020DB: TCDAPISV1020DB;
      aMetadataCache: TMetadataCache);
  end;

implementation

constructor TDataAdapter.Create(aSV1020DB: TCDAPISV1020DB;
  aMetadataCache: TMetadataCache);
begin
  inherited Create;
  FSV1020DB := ASV1020DB;
  FMetadataCache := AMetadataCache;
  FObject := TObject.Create;
end;

function TDataAdapter.GetAllowedFields(aUsageId: Integer): TFieldNameMapArray;
var
  usageDef: TUsageDef;
  found: Boolean;
  i, count: Integer;
begin
  Result := nil;
  FMetadataCache.FindUsageById(AUsageId, UsageDef, Found);
  if not Found then
    raise EUsageNotFound.CreateFmt('Usage %d not found in cache', [AUsageId]);

  Count := 0;
  for I := 0 to High(UsageDef.fields) do
  begin
    { Skip lookup fields and fields with no physical column }
    if UsageDef.fields[I].isLookup then
      Continue;
    if UsageDef.fields[I].physicalFieldName = '' then
      Continue;
    SetLength(Result, Count + 1);
    Result[Count].DatasetName := UpperCase(UsageDef.fields[I].datasetFieldName);
    Result[Count].PhysicalName := UpperCase(UsageDef.fields[I].physicalFieldName);
    Result[Count].FieldType := UsageDef.fields[I].fieldType;
    Inc(Count);
  end;
end;

function TDataAdapter.RequeryRow(aConn: TSqlDBConnection;
  const aUsageDef: TUsageDef;
  const aAllowedFields: TFieldNameMapArray;
  const aPKFields: TRawUtf8DynArray;
  const aPKValues: TDocVariantData): RawJson;
var
  baseSql, sql, wherePart: RawUtf8;
  stmt: ISqlDBStatement;
  jsonStream: TRawByteStringStream;
  json: RawUtf8;
  doc: TDocVariantData;
  i, paramIdx, fieldType, mapIdx: Integer;
  pkValue: Variant;
begin
  Result := '';

  baseSql := FMetadataCache.GetSqlText(aUsageDef.usageId);
  if baseSql = '' then
    Exit('{}'); { SQL not in cache — mutation already succeeded, return empty }

  { Build WHERE clause for PK fields }
  wherePart := '';
  for I := 0 to High(APKFields) do
  begin
    if I > 0 then
      wherePart := wherePart + ' AND ';
    wherePart := wherePart + 'sub_q.' + aPKFields[I] + ' = ?';
  end;

  FormatUtf8('SELECT * FROM (%) sub_q WHERE %',
    [baseSql, wherePart], Sql);

  stmt := AConn.NewStatementPrepared(Sql, True);
  ParamIdx := 1;
  for I := 0 to High(APKFields) do
  begin
    { Look up field type from allowed fields map }
    FieldType := 0;
    for MapIdx := 0 to High(AAllowedFields) do
      if AAllowedFields[MapIdx].PhysicalName = APKFields[I] then
      begin
        FieldType := AAllowedFields[MapIdx].FieldType;
        Break;
      end;
    PKValue := APKValues.Value[APKFields[I]];
    BindVariantParam(Stmt, ParamIdx, PKValue, FieldType);
    Inc(ParamIdx);
  end;
  Stmt.ExecutePrepared;

  JsonStream := TRawByteStringStream.Create;
  try
    Stmt.FetchAllToJson(JsonStream, True);
    Json := JsonStream.DataString;
  finally
    JsonStream.Free;
  end;

  (* FetchAllToJson returns [{...}] — extract the first element *)
  Doc.InitJson(Json, JSON_FAST);
  if Doc.Count > 0 then
    Result := VariantSaveJson(Doc.Values[0])
  else
    Result := '{}';
end;

end.
