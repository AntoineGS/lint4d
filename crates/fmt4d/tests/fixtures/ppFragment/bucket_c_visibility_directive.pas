unit BucketCVisibility;

interface

type
  TFoo = class(TObject)
  public
    procedure Assign(Source: TObject);
  {$IFDEF UNITTEST}
  published
  {$ENDIF}
    property Bar: integer read FBar;
  end;

implementation

end.
