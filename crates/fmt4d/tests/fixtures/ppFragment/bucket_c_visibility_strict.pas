unit BucketCVisibilityStrict;

interface

type
  TFoo = class(TObject)
  public
    procedure Assign(Source: TObject);
  {$IFDEF UNITTEST}
  strict private
  {$ENDIF}
    FBar: integer;
  end;

implementation

end.
